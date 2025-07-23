// Copyright 2018-2025 the Deno authors. MIT license.
use std::borrow::Cow;
use std::cell::RefCell;
use std::ffi::c_void;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::ptr::null;
use std::rc::Rc;
use std::sync::OnceLock;
use std::task::Poll;
use std::task::ready;

use cache_control::CacheControl;
use deno_core::AsyncRefCell;
use deno_core::AsyncResult;
use deno_core::BufView;
use deno_core::ByteString;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::CancelTryFuture;
use deno_core::ExternalPointer;
use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::external;
use deno_core::futures::TryFutureExt;
use deno_core::op2;
use deno_core::serde_v8::from_v8;
use deno_core::unsync::JoinHandle;
use deno_core::unsync::spawn;
use deno_core::v8;
use deno_core::v8_static_strings;
use deno_error::JsErrorBox;
use deno_error::JsErrorClass;
use deno_net::ops_tls::TlsStream;
use deno_net::raw::NetworkStream;
use deno_net::resolve_addr::resolve_addr_sync;
use deno_net::tcp::TcpListener;
use deno_websocket::ws_create_server_stream;
use fly_accept_encoding::Encoding;
use http::response::Parts;
use hyper::StatusCode;
use hyper::body::Body;
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::body::SizeHint;
use hyper::header::ACCEPT_ENCODING;
use hyper::header::CACHE_CONTROL;
use hyper::header::CONTENT_ENCODING;
use hyper::header::CONTENT_LENGTH;
use hyper::header::CONTENT_RANGE;
use hyper::header::CONTENT_TYPE;
use hyper::header::COOKIE;
use hyper::header::HeaderMap;
use hyper::http::HeaderName;
use hyper::http::HeaderValue;
use hyper::server::conn::http1;
use hyper::server::conn::http2;
use hyper::service::HttpService;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use once_cell::sync::Lazy;
use smallvec::SmallVec;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::fly_accept_encoding;
use crate::LocalExecutor;
use crate::Options;
use crate::compressible::is_content_compressible;
use crate::extract_network_stream;
use crate::network_buffered_stream::NetworkStreamPrefixCheck;
use crate::request_body::HttpRequestBody;
use crate::request_properties::HttpConnectionProperties;
use crate::request_properties::HttpListenProperties;
use crate::request_properties::HttpPropertyExtractor;
use crate::response_body::Compression;
use crate::response_body::ResponseBytesInner;
use crate::response_body::ResponseStreamResult;
use crate::service::HttpRecord;
use crate::service::HttpRecordResponse;
use crate::service::HttpRequestBodyAutocloser;
use crate::service::HttpServerState;
use crate::service::SignallingRc;
use crate::service::handle_request;
use crate::service::http_general_trace;
use crate::service::http_trace;
use crate::websocket_upgrade::WebSocketUpgrade;

type Request = hyper::Request<Incoming>;

static USE_WRITEV: Lazy<bool> = Lazy::new(|| {
  let enable = std::env::var("DENO_USE_WRITEV").ok();

  if let Some(val) = enable {
    return !val.is_empty();
  }

  false
});

/// All HTTP/2 connections start with this byte string.
///
/// In HTTP/2, each endpoint is required to send a connection preface as a final confirmation
/// of the protocol in use and to establish the initial settings for the HTTP/2 connection. The
/// client and server each send a different connection preface.
///
/// The client connection preface starts with a sequence of 24 octets, which in hex notation is:
///
/// 0x505249202a20485454502f322e300d0a0d0a534d0d0a0d0a
///
/// That is, the connection preface starts with the string PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n). This sequence
/// MUST be followed by a SETTINGS frame (Section 6.5), which MAY be empty.
const HTTP2_PREFIX: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// ALPN negotiation for "h2"
const TLS_ALPN_HTTP_2: &[u8] = b"h2";

/// ALPN negotiation for "http/1.1"
const TLS_ALPN_HTTP_11: &[u8] = b"http/1.1";

/// Name a trait for streams we can serve HTTP over.
trait HttpServeStream:
  tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static
{
}
impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>
  HttpServeStream for S
{
}

#[repr(transparent)]
struct RcHttpRecord(Rc<HttpRecord>);

// Register the [`HttpRecord`] as an external.
external!(RcHttpRecord, "http record");

/// Construct Rc<HttpRecord> from raw external pointer, consuming
/// refcount. You must make sure the external is deleted on the JS side.
macro_rules! take_external {
  ($external:expr, $args:tt) => {{
    let ptr = ExternalPointer::<RcHttpRecord>::from_raw($external);
    let record = ptr.unsafely_take().0;
    http_trace!(record, $args);
    record
  }};
}

/// Clone Rc<HttpRecord> from raw external pointer.
macro_rules! clone_external {
  ($external:expr, $args:tt) => {{
    let ptr = ExternalPointer::<RcHttpRecord>::from_raw($external);
    ptr.unsafely_deref().0.clone()
  }};
}

#[derive(Debug, thiserror::Error, deno_error::JsError)]
pub enum HttpNextError {
  #[class(inherit)]
  #[error(transparent)]
  Resource(#[from] deno_core::error::ResourceError),
  #[class(inherit)]
  #[error("{0}")]
  Io(#[from] io::Error),
  #[class(inherit)]
  #[error(transparent)]
  WebSocketUpgrade(crate::websocket_upgrade::WebSocketUpgradeError),
  #[class("Http")]
  #[error("{0}")]
  Hyper(#[from] hyper::Error),
  #[class(inherit)]
  #[error(transparent)]
  JoinError(
    #[from]
    #[inherit]
    tokio::task::JoinError,
  ),
  #[class(inherit)]
  #[error(transparent)]
  Canceled(
    #[from]
    #[inherit]
    deno_core::Canceled,
  ),
  #[class(generic)]
  #[error(transparent)]
  UpgradeUnavailable(#[from] crate::service::UpgradeUnavailableError),
  #[class(inherit)]
  #[error("{0}")]
  Other(
    #[from]
    #[inherit]
    deno_error::JsErrorBox,
  ),
}

#[op2(fast)]
#[smi]
pub fn op_http_upgrade_raw(
  state: &mut OpState,
  external: *const c_void,
) -> Result<ResourceId, HttpNextError> {
  // SAFETY: external is deleted before calling this op.
  let http = unsafe { take_external!(external, "op_http_upgrade_raw") };

  // Stage 1: extract the upgrade future
  let upgrade = http.upgrade()?;
  let (read, write) = tokio::io::duplex(1024);
  let (read_rx, write_tx) = tokio::io::split(read);
  let (mut write_rx, mut read_tx) = tokio::io::split(write);
  spawn(async move {
    let mut upgrade_stream = WebSocketUpgrade::<()>::default();

    // Stage 2: Extract the Upgraded connection
    let mut buf = [0; 1024];
    let upgraded = loop {
      let read = Pin::new(&mut write_rx).read(&mut buf).await?;
      match upgrade_stream.write(&buf[..read]) {
        Ok(None) => continue,
        Ok(Some((response, bytes))) => {
          let (response_parts, _) = response.into_parts();
          *http.response_parts() = response_parts;
          http.complete();
          let mut upgraded = TokioIo::new(upgrade.await?);
          upgraded.write_all(&bytes).await?;
          break upgraded;
        }
        Err(err) => return Err(HttpNextError::WebSocketUpgrade(err)),
      }
    };

    // Stage 3: Pump the data
    let (mut upgraded_rx, mut upgraded_tx) = tokio::io::split(upgraded);

    spawn(async move {
      let mut buf = [0; 1024];
      loop {
        let read = upgraded_rx.read(&mut buf).await?;
        if read == 0 {
          break;
        }
        read_tx.write_all(&buf[..read]).await?;
      }
      Ok::<_, HttpNextError>(())
    });
    spawn(async move {
      let mut buf = [0; 1024];
      loop {
        let read = write_rx.read(&mut buf).await?;
        if read == 0 {
          break;
        }
        upgraded_tx.write_all(&buf[..read]).await?;
      }
      Ok::<_, HttpNextError>(())
    });

    Ok(())
  });

  Ok(
    state
      .resource_table
      .add(UpgradeStream::new(read_rx, write_tx)),
  )
}

#[op2(async)]
#[smi]
pub async fn op_http_upgrade_websocket_next(
  state: Rc<RefCell<OpState>>,
  external: *const c_void,
  #[serde] headers: Vec<(ByteString, ByteString)>,
) -> Result<ResourceId, HttpNextError> {
  let http =
    // SAFETY: external is deleted before calling this op.
    unsafe { take_external!(external, "op_http_upgrade_websocket_next") };
  // Stage 1: set the response to 101 Switching Protocols and send it
  let upgrade = http.upgrade()?;
  {
    {
      http.otel_info_set_status(StatusCode::SWITCHING_PROTOCOLS.as_u16());
    }
    let mut response_parts = http.response_parts();
    response_parts.status = StatusCode::SWITCHING_PROTOCOLS;

    for (name, value) in headers {
      response_parts.headers.append(
        HeaderName::from_bytes(&name).unwrap(),
        HeaderValue::from_bytes(&value).unwrap(),
      );
    }
  }
  http.complete();

  // Stage 2: wait for the request to finish upgrading
  let upgraded = upgrade.await?;

  // Stage 3: take the extracted raw network stream and upgrade it to a websocket, then return it
  let (stream, bytes) = extract_network_stream(upgraded);
  Ok(ws_create_server_stream(
    &mut state.borrow_mut(),
    stream,
    bytes,
  ))
}

#[op2(fast)]
pub fn op_http_set_promise_complete(external: *const c_void, status: u16) {
  let http =
    // SAFETY: external is deleted before calling this op.
    unsafe { take_external!(external, "op_http_set_promise_complete") };
  set_promise_complete(http, status);
}

fn set_promise_complete(http: Rc<HttpRecord>, status: u16) {
  // The Javascript code should never provide a status that is invalid here (see 23_response.js), so we
  // will quietly ignore invalid values.
  if let Ok(code) = StatusCode::from_u16(status) {
    {
      http.response_parts().status = code;
    }
    http.otel_info_set_status(status);
  }
  http.complete();
}

#[op2]
pub fn op_http_get_request_method_and_url<'scope, HTTP>(
  scope: &mut v8::HandleScope<'scope>,
  external: *const c_void,
) -> v8::Local<'scope, v8::Array>
where
  HTTP: HttpPropertyExtractor,
{
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_get_request_method_and_url") };
  let request_info = http.request_info();
  let request_parts = http.request_parts();
  let request_properties = HTTP::request_properties(
    &request_info,
    &request_parts.uri,
    &request_parts.headers,
  );

  let method: v8::Local<v8::Value> = v8::String::new_from_utf8(
    scope,
    request_parts.method.as_str().as_bytes(),
    v8::NewStringType::Normal,
  )
  .unwrap()
  .into();

  let scheme: v8::Local<v8::Value> = match request_parts.uri.scheme_str() {
    Some(scheme) => v8::String::new_from_utf8(
      scope,
      scheme.as_bytes(),
      v8::NewStringType::Normal,
    )
    .unwrap()
    .into(),
    None => v8::undefined(scope).into(),
  };

  let authority: v8::Local<v8::Value> =
    if let Some(authority) = request_parts.uri.authority() {
      v8::String::new_from_utf8(
        scope,
        authority.as_str().as_ref(),
        v8::NewStringType::Normal,
      )
      .unwrap()
      .into()
    } else if let Some(authority) = request_properties.authority {
      v8::String::new_from_utf8(
        scope,
        authority.as_bytes(),
        v8::NewStringType::Normal,
      )
      .unwrap()
      .into()
    } else {
      v8::undefined(scope).into()
    };

  // Only extract the path part - we handle authority elsewhere
  let path = match request_parts.uri.path_and_query() {
    Some(path_and_query) => {
      let path = path_and_query.as_str();
      if matches!(path.as_bytes().first(), Some(b'/' | b'*')) {
        Cow::Borrowed(path)
      } else {
        Cow::Owned(format!("/{}", path))
      }
    }
    None => Cow::Borrowed(""),
  };

  let path: v8::Local<v8::Value> = v8::String::new_from_utf8(
    scope,
    path.as_bytes(),
    v8::NewStringType::Normal,
  )
  .unwrap()
  .into();

  let (peer_ip, peer_port) = match &*http.client_addr() {
    Some(client_addr) => {
      let addr: std::net::SocketAddr =
        client_addr.to_str().unwrap().parse().unwrap();
      (Rc::from(format!("{}", addr.ip())), Some(addr.port() as u32))
    }
    _ => (request_info.peer_address.clone(), request_info.peer_port),
  };

  let peer_ip: v8::Local<v8::Value> = v8::String::new_from_utf8(
    scope,
    peer_ip.as_bytes(),
    v8::NewStringType::Normal,
  )
  .unwrap()
  .into();

  let peer_port: v8::Local<v8::Value> = match peer_port {
    Some(port) => v8::Number::new(scope, port.into()).into(),
    None => v8::undefined(scope).into(),
  };

  let vec = [method, authority, path, peer_ip, peer_port, scheme];
  v8::Array::new_with_elements(scope, vec.as_slice())
}

#[op2]
#[serde]
pub fn op_http_get_request_header(
  external: *const c_void,
  #[string] name: String,
) -> Option<ByteString> {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_get_request_header") };
  let request_parts = http.request_parts();
  let value = request_parts.headers.get(name);
  value.map(|value| value.as_bytes().into())
}

#[op2]
pub fn op_http_get_request_headers<'scope>(
  scope: &mut v8::HandleScope<'scope>,
  external: *const c_void,
) -> v8::Local<'scope, v8::Array> {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_get_request_headers") };
  let headers = &http.request_parts().headers;
  // Two slots for each header key/value pair
  let mut vec: SmallVec<[v8::Local<v8::Value>; 32]> =
    SmallVec::with_capacity(headers.len() * 2);

  let mut cookies: Option<Vec<&[u8]>> = None;
  for (name, value) in headers {
    if name == COOKIE {
      if let Some(ref mut cookies) = cookies {
        cookies.push(value.as_bytes());
      } else {
        cookies = Some(vec![value.as_bytes()]);
      }
    } else {
      vec.push(
        v8::String::new_from_one_byte(
          scope,
          name.as_ref(),
          v8::NewStringType::Normal,
        )
        .unwrap()
        .into(),
      );
      vec.push(
        v8::String::new_from_one_byte(
          scope,
          value.as_bytes(),
          v8::NewStringType::Normal,
        )
        .unwrap()
        .into(),
      );
    }
  }

  // We treat cookies specially, because we don't want them to get them
  // mangled by the `Headers` object in JS. What we do is take all cookie
  // headers and concat them into a single cookie header, separated by
  // semicolons.
  // TODO(mmastrac): This should probably happen on the JS side on-demand
  if let Some(cookies) = cookies {
    let cookie_sep = "; ".as_bytes();

    vec.push(
      v8::String::new_external_onebyte_static(scope, COOKIE.as_ref())
        .unwrap()
        .into(),
    );
    vec.push(
      v8::String::new_from_one_byte(
        scope,
        cookies.join(cookie_sep).as_ref(),
        v8::NewStringType::Normal,
      )
      .unwrap()
      .into(),
    );
  }

  v8::Array::new_with_elements(scope, vec.as_slice())
}

#[op2(fast)]
#[smi]
pub fn op_http_read_request_body(
  state: Rc<RefCell<OpState>>,
  external: *const c_void,
) -> ResourceId {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_read_request_body") };
  let rid = match http.take_request_body() {
    Some(incoming) => {
      let body_resource = Rc::new(HttpRequestBody::new(incoming));
      state.borrow_mut().resource_table.add_rc(body_resource)
    }
    _ => {
      // This should not be possible, but rather than panicking we'll return an invalid
      // resource value to JavaScript.
      ResourceId::MAX
    }
  };
  http.put_resource(HttpRequestBodyAutocloser::new(rid, state.clone()));
  rid
}

#[op2(fast)]
pub fn op_http_set_response_header(
  external: *const c_void,
  #[string(onebyte)] name: Cow<[u8]>,
  #[string(onebyte)] value: Cow<[u8]>,
) {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_set_response_header") };
  let mut response_parts = http.response_parts();
  // These are valid latin-1 strings
  let name = HeaderName::from_bytes(&name).unwrap();
  let value = match value {
    Cow::Borrowed(bytes) => HeaderValue::from_bytes(bytes).unwrap(),
    // SAFETY: These are valid latin-1 strings
    Cow::Owned(bytes_vec) => unsafe {
      HeaderValue::from_maybe_shared_unchecked(bytes::Bytes::from(bytes_vec))
    },
  };
  response_parts.headers.append(name, value);
}

#[op2(fast)]
pub fn op_http_set_response_headers(
  scope: &mut v8::HandleScope,
  external: *const c_void,
  headers: v8::Local<v8::Array>,
) {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_set_response_headers") };
  // TODO(mmastrac): Invalid headers should be handled?
  let mut response_parts = http.response_parts();

  let len = headers.length();
  let header_len = len * 2;
  response_parts
    .headers
    .reserve(header_len.try_into().unwrap());

  for i in 0..len {
    let item = headers.get_index(scope, i).unwrap();
    let pair = v8::Local::<v8::Array>::try_from(item).unwrap();
    let name = pair.get_index(scope, 0).unwrap();
    let value = pair.get_index(scope, 1).unwrap();

    let v8_name: ByteString = from_v8(scope, name).unwrap();
    let v8_value: ByteString = from_v8(scope, value).unwrap();
    let header_name = HeaderName::from_bytes(&v8_name).unwrap();
    let header_value =
      // SAFETY: These are valid latin-1 strings
      unsafe { HeaderValue::from_maybe_shared_unchecked(v8_value) };
    response_parts.headers.append(header_name, header_value);
  }
}

#[op2]
pub fn op_http_set_response_trailers(
  external: *const c_void,
  #[serde] trailers: Vec<(ByteString, ByteString)>,
) {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_set_response_trailers") };
  let mut trailer_map: HeaderMap = HeaderMap::with_capacity(trailers.len());
  for (name, value) in trailers {
    // These are valid latin-1 strings
    let name = HeaderName::from_bytes(&name).unwrap();
    // SAFETY: These are valid latin-1 strings
    let value = unsafe { HeaderValue::from_maybe_shared_unchecked(value) };
    trailer_map.append(name, value);
  }
  *http.trailers() = Some(trailer_map);
}

fn is_request_compressible(
  length: Option<usize>,
  headers: &HeaderMap,
) -> Compression {
  if let Some(length) = length {
    // By the time we add compression headers and Accept-Encoding, it probably doesn't make sense
    // to compress stuff that's smaller than this.
    if length < 64 {
      return Compression::None;
    }
  }

  let Some(accept_encoding) = headers.get(ACCEPT_ENCODING) else {
    return Compression::None;
  };

  match accept_encoding.to_str() {
    // Firefox and Chrome send this -- no need to parse
    Ok("gzip, deflate, br") => return Compression::Brotli,
    Ok("gzip, deflate, br, zstd") => return Compression::Brotli,
    Ok("gzip") => return Compression::GZip,
    Ok("br") => return Compression::Brotli,
    _ => (),
  }

  // Fall back to the expensive parser
  let accepted =
    fly_accept_encoding::encodings_iter_http_1(headers).filter(|r| {
      matches!(
        r,
        Ok((
          Some(Encoding::Identity | Encoding::Gzip | Encoding::Brotli),
          _
        ))
      )
    });
  match fly_accept_encoding::preferred(accepted) {
    Ok(Some(fly_accept_encoding::Encoding::Gzip)) => Compression::GZip,
    Ok(Some(fly_accept_encoding::Encoding::Brotli)) => Compression::Brotli,
    _ => Compression::None,
  }
}

fn is_response_compressible(headers: &HeaderMap) -> bool {
  if let Some(content_type) = headers.get(CONTENT_TYPE) {
    if !is_content_compressible(content_type) {
      return false;
    }
  } else {
    return false;
  }
  if headers.contains_key(CONTENT_ENCODING) {
    return false;
  }
  if headers.contains_key(CONTENT_RANGE) {
    return false;
  }
  if let Some(cache_control) = headers.get(CACHE_CONTROL) {
    if let Ok(s) = std::str::from_utf8(cache_control.as_bytes()) {
      if let Some(cache_control) = CacheControl::from_value(s) {
        if cache_control.no_transform {
          return false;
        }
      }
    }
  }
  true
}

fn modify_compressibility_from_response(
  compression: Compression,
  headers: &mut HeaderMap,
) -> Compression {
  ensure_vary_accept_encoding(headers);
  if compression == Compression::None {
    return Compression::None;
  }
  if !is_response_compressible(headers) {
    return Compression::None;
  }
  let encoding = match compression {
    Compression::Brotli => "br",
    Compression::GZip => "gzip",
    _ => unreachable!(),
  };
  weaken_etag(headers);
  headers.remove(CONTENT_LENGTH);
  headers.insert(CONTENT_ENCODING, HeaderValue::from_static(encoding));
  compression
}

/// If the user provided a ETag header for uncompressed data, we need to ensure it is a
/// weak Etag header ("W/").
fn weaken_etag(hmap: &mut HeaderMap) {
  if let Some(etag) = hmap.get_mut(hyper::header::ETAG) {
    if !etag.as_bytes().starts_with(b"W/") {
      let mut v = Vec::with_capacity(etag.as_bytes().len() + 2);
      v.extend(b"W/");
      v.extend(etag.as_bytes());
      *etag = v.try_into().unwrap();
    }
  }
}

// Set Vary: Accept-Encoding header for direct body response.
// Note: we set the header irrespective of whether or not we compress the data
// to make sure cache services do not serve uncompressed data to clients that
// support compression.
fn ensure_vary_accept_encoding(hmap: &mut HeaderMap) {
  if let Some(v) = hmap.get_mut(hyper::header::VARY) {
    if let Ok(s) = v.to_str() {
      if !s.to_lowercase().contains("accept-encoding") {
        *v = format!("Accept-Encoding, {s}").try_into().unwrap()
      }
      return;
    }
  }
  hmap.insert(
    hyper::header::VARY,
    HeaderValue::from_static("Accept-Encoding"),
  );
}

/// Sets the appropriate response body. Use `force_instantiate_body` if you need
/// to ensure that the response is cleaned up correctly (eg: for resources).
fn set_response(
  http: Rc<HttpRecord>,
  length: Option<usize>,
  status: u16,
  force_instantiate_body: bool,
  response_fn: impl FnOnce(Compression) -> ResponseBytesInner,
) {
  // The request may have been cancelled by this point and if so, there's no need for us to
  // do all of this work to send the response.
  if !http.cancelled() {
    let compression =
      is_request_compressible(length, &http.request_parts().headers);
    let mut response_headers =
      std::cell::RefMut::map(http.response_parts(), |this| &mut this.headers);
    let compression =
      modify_compressibility_from_response(compression, &mut response_headers);
    drop(response_headers);
    http.set_response_body(response_fn(compression));

    // The Javascript code should never provide a status that is invalid here (see 23_response.js), so we
    // will quietly ignore invalid values.
    if let Ok(code) = StatusCode::from_u16(status) {
      {
        http.response_parts().status = code;
      }
      http.otel_info_set_status(status);
    }
  } else if force_instantiate_body {
    response_fn(Compression::None).abort();
  }

  http.complete();
}

#[op2(fast)]
pub fn op_http_get_request_cancelled(external: *const c_void) -> bool {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_get_request_cancelled") };
  http.cancelled()
}

#[op2(async)]
pub async fn op_http_request_on_cancel(external: *const c_void) -> bool {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_request_on_cancel") };
  let (tx, rx) = tokio::sync::oneshot::channel();

  http.on_cancel(tx);
  drop(http);

  rx.await.is_ok()
}

/// Returned promise resolves when body streaming finishes.
/// Call [`op_http_close_after_finish`] when done with the external.
#[op2(async)]
pub async fn op_http_set_response_body_resource(
  state: Rc<RefCell<OpState>>,
  external: *const c_void,
  #[smi] stream_rid: ResourceId,
  auto_close: bool,
  status: u16,
) -> Result<bool, HttpNextError> {
  let http =
    // SAFETY: op is called with external.
    unsafe { clone_external!(external, "op_http_set_response_body_resource") };

  // IMPORTANT: We might end up requiring the OpState lock in set_response if we need to drop the request
  // body resource so we _cannot_ hold the OpState lock longer than necessary.

  // If the stream is auto_close, we will hold the last ref to it until the response is complete.
  // TODO(mmastrac): We should be using the same auto-close functionality rather than removing autoclose resources.
  // It's possible things could fail elsewhere if code expects the rid to continue existing after the response has been
  // returned.
  let resource = {
    let mut state = state.borrow_mut();
    if auto_close {
      state.resource_table.take_any(stream_rid)?
    } else {
      state.resource_table.get_any(stream_rid)?
    }
  };

  *http.needs_close_after_finish() = true;

  set_response(
    http.clone(),
    resource.size_hint().1.map(|s| s as usize),
    status,
    true,
    move |compression| {
      ResponseBytesInner::from_resource(compression, resource, auto_close)
    },
  );

  Ok(http.response_body_finished().await)
}

#[op2(fast)]
pub fn op_http_close_after_finish(external: *const c_void) {
  let http =
    // SAFETY: external is deleted before calling this op.
    unsafe { take_external!(external, "op_http_close_after_finish") };
  http.close_after_finish();
}

#[op2(fast)]
pub fn op_http_set_response_body_text(
  external: *const c_void,
  #[string] text: String,
  status: u16,
) {
  let http =
    // SAFETY: external is deleted before calling this op.
    unsafe { take_external!(external, "op_http_set_response_body_text") };
  if !text.is_empty() {
    set_response(http, Some(text.len()), status, false, |compression| {
      ResponseBytesInner::from_vec(compression, text.into_bytes())
    });
  } else {
    set_promise_complete(http, status);
  }
}

#[op2]
pub fn op_http_set_response_body_bytes(
  external: *const c_void,
  #[buffer] buffer: JsBuffer,
  status: u16,
) {
  let http =
    // SAFETY: external is deleted before calling this op.
    unsafe { take_external!(external, "op_http_set_response_body_bytes") };
  if !buffer.is_empty() {
    set_response(http, Some(buffer.len()), status, false, |compression| {
      ResponseBytesInner::from_bufview(compression, BufView::from(buffer))
    });
  } else {
    set_promise_complete(http, status);
  }
}

fn serve_http11_unconditional<B: Body + 'static>(
  io: impl HttpServeStream,
  svc: impl HttpService<Incoming, ResBody = B> + 'static,
  cancel: Rc<CancelHandle>,
  http1_builder_hook: Option<fn(http1::Builder) -> http1::Builder>,
) -> impl Future<Output = Result<(), hyper::Error>> + 'static
where
  B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
  let mut builder = http1::Builder::new();
  builder.keep_alive(true).writev(*USE_WRITEV);

  if let Some(http1_builder_hook) = http1_builder_hook {
    builder = http1_builder_hook(builder);
  }

  let conn = builder
    .serve_connection(TokioIo::new(io), svc)
    .with_upgrades();

  async {
    match conn.or_abort(cancel).await {
      Err(mut conn) => {
        Pin::new(&mut conn).graceful_shutdown();
        conn.await
      }
      Ok(res) => res,
    }
  }
}

fn serve_http2_unconditional<B: Body + 'static>(
  io: impl HttpServeStream,
  svc: impl HttpService<Incoming, ResBody = B> + 'static,
  cancel: Rc<CancelHandle>,
  http2_builder_hook: Option<
    fn(http2::Builder<LocalExecutor>) -> http2::Builder<LocalExecutor>,
  >,
) -> impl Future<Output = Result<(), hyper::Error>> + 'static
where
  B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
  let mut builder = http2::Builder::new(LocalExecutor);

  if let Some(http2_builder_hook) = http2_builder_hook {
    builder = http2_builder_hook(builder);
  }

  let conn = builder.serve_connection(TokioIo::new(io), svc);
  async {
    match conn.or_abort(cancel).await {
      Err(mut conn) => {
        Pin::new(&mut conn).graceful_shutdown();
        conn.await
      }
      Ok(res) => res,
    }
  }
}

async fn serve_http2_autodetect<B: Body + 'static>(
  io: impl HttpServeStream,
  svc: impl HttpService<Incoming, ResBody = B> + 'static,
  cancel: Rc<CancelHandle>,
  options: Options,
) -> Result<(), HttpNextError>
where
  B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
  let prefix = NetworkStreamPrefixCheck::new(io, HTTP2_PREFIX);
  let (matches, io) = prefix.match_prefix().await?;
  if matches {
    serve_http2_unconditional(io, svc, cancel, options.http2_builder_hook)
      .await
      .map_err(HttpNextError::Hyper)
  } else {
    serve_http11_unconditional(io, svc, cancel, options.http1_builder_hook)
      .await
      .map_err(HttpNextError::Hyper)
  }
}

fn serve_https(
  mut io: TlsStream<TcpStream>,
  request_info: HttpConnectionProperties,
  lifetime: HttpLifetime,
  tx: tokio::sync::mpsc::Sender<Rc<HttpRecord>>,
  options: Options,
) -> JoinHandle<Result<(), HttpNextError>> {
  let HttpLifetime {
    server_state,
    connection_cancel_handle,
    listen_cancel_handle,
  } = lifetime;

  let legacy_abort = !options.no_legacy_abort;
  let svc = service_fn(move |req: Request| {
    handle_request(
      req,
      request_info.clone(),
      server_state.clone(),
      tx.clone(),
      legacy_abort,
    )
  });
  spawn(
    async move {
      let handshake = io.handshake().await?;
      // If the client specifically negotiates a protocol, we will use it. If not, we'll auto-detect
      // based on the prefix bytes
      let handshake = handshake.alpn;
      if Some(TLS_ALPN_HTTP_2) == handshake.as_deref() {
        serve_http2_unconditional(
          io,
          svc,
          listen_cancel_handle,
          options.http2_builder_hook,
        )
        .await
        .map_err(HttpNextError::Hyper)
      } else if Some(TLS_ALPN_HTTP_11) == handshake.as_deref() {
        serve_http11_unconditional(
          io,
          svc,
          listen_cancel_handle,
          options.http1_builder_hook,
        )
        .await
        .map_err(HttpNextError::Hyper)
      } else {
        serve_http2_autodetect(io, svc, listen_cancel_handle, options).await
      }
    }
    .try_or_cancel(connection_cancel_handle),
  )
}

fn serve_http(
  io: impl HttpServeStream,
  request_info: HttpConnectionProperties,
  lifetime: HttpLifetime,
  tx: tokio::sync::mpsc::Sender<Rc<HttpRecord>>,
  options: Options,
) -> JoinHandle<Result<(), HttpNextError>> {
  let HttpLifetime {
    server_state,
    connection_cancel_handle,
    listen_cancel_handle,
  } = lifetime;

  let legacy_abort = !options.no_legacy_abort;
  let svc = service_fn(move |req: Request| {
    handle_request(
      req,
      request_info.clone(),
      server_state.clone(),
      tx.clone(),
      legacy_abort,
    )
  });
  spawn(
    serve_http2_autodetect(io, svc, listen_cancel_handle, options)
      .try_or_cancel(connection_cancel_handle),
  )
}

fn serve_http_on<HTTP>(
  connection: HTTP::Connection,
  listen_properties: &HttpListenProperties,
  lifetime: HttpLifetime,
  tx: tokio::sync::mpsc::Sender<Rc<HttpRecord>>,
  options: Options,
) -> JoinHandle<Result<(), HttpNextError>>
where
  HTTP: HttpPropertyExtractor,
{
  let connection_properties: HttpConnectionProperties =
    HTTP::connection_properties(listen_properties, &connection);

  let network_stream = HTTP::to_network_stream_from_connection(connection);

  match network_stream {
    NetworkStream::Tcp(conn) => {
      serve_http(conn, connection_properties, lifetime, tx, options)
    }
    NetworkStream::Tls(conn) => {
      serve_https(conn, connection_properties, lifetime, tx, options)
    }
    #[cfg(unix)]
    NetworkStream::Unix(conn) => {
      serve_http(conn, connection_properties, lifetime, tx, options)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    NetworkStream::Vsock(conn) => {
      serve_http(conn, connection_properties, lifetime, tx, options)
    }
    NetworkStream::Tunnel(conn) => {
      serve_http(conn, connection_properties, lifetime, tx, options)
    }
  }
}

#[derive(Clone)]
struct HttpLifetime {
  connection_cancel_handle: Rc<CancelHandle>,
  listen_cancel_handle: Rc<CancelHandle>,
  server_state: SignallingRc<HttpServerState>,
}

struct HttpJoinHandle {
  join_handle: AsyncRefCell<Option<JoinHandle<Result<(), HttpNextError>>>>,
  connection_cancel_handle: Rc<CancelHandle>,
  listen_cancel_handle: Rc<CancelHandle>,
  rx: AsyncRefCell<tokio::sync::mpsc::Receiver<Rc<HttpRecord>>>,
  server_state: SignallingRc<HttpServerState>,
}

impl HttpJoinHandle {
  fn new(rx: tokio::sync::mpsc::Receiver<Rc<HttpRecord>>) -> Self {
    Self {
      join_handle: AsyncRefCell::new(None),
      connection_cancel_handle: CancelHandle::new_rc(),
      listen_cancel_handle: CancelHandle::new_rc(),
      rx: AsyncRefCell::new(rx),
      server_state: HttpServerState::new(),
    }
  }

  fn lifetime(self: &Rc<Self>) -> HttpLifetime {
    HttpLifetime {
      connection_cancel_handle: self.connection_cancel_handle.clone(),
      listen_cancel_handle: self.listen_cancel_handle.clone(),
      server_state: self.server_state.clone(),
    }
  }

  fn connection_cancel_handle(self: &Rc<Self>) -> Rc<CancelHandle> {
    self.connection_cancel_handle.clone()
  }

  fn listen_cancel_handle(self: &Rc<Self>) -> Rc<CancelHandle> {
    self.listen_cancel_handle.clone()
  }
}

impl Resource for HttpJoinHandle {
  fn name(&self) -> Cow<str> {
    "http".into()
  }

  fn close(self: Rc<Self>) {
    // During a close operation, we cancel everything
    self.connection_cancel_handle.cancel();
    self.listen_cancel_handle.cancel();
  }
}

impl Drop for HttpJoinHandle {
  fn drop(&mut self) {
    // In some cases we may be dropped without closing, so let's cancel everything on the way out
    self.connection_cancel_handle.cancel();
    self.listen_cancel_handle.cancel();
  }
}

#[op2]
#[serde]
pub fn op_http_serve<HTTP>(
  state: Rc<RefCell<OpState>>,
  #[smi] listener_rid: ResourceId,
) -> Result<(ResourceId, &'static str, String, bool), HttpNextError>
where
  HTTP: HttpPropertyExtractor,
{
  let listener =
    HTTP::get_listener_for_rid(&mut state.borrow_mut(), listener_rid)?;

  let listen_properties = HTTP::listen_properties_from_listener(&listener)?;

  let (tx, rx) = tokio::sync::mpsc::channel(10);
  let resource: Rc<HttpJoinHandle> = Rc::new(HttpJoinHandle::new(rx));
  let listen_cancel_clone = resource.listen_cancel_handle();

  let lifetime = resource.lifetime();

  let options = {
    let state = state.borrow();
    *state.borrow::<Options>()
  };

  let listen_properties_clone: HttpListenProperties = listen_properties.clone();
  let handle = spawn(async move {
    loop {
      let conn = HTTP::accept_connection_from_listener(&listener)
        .try_or_cancel(listen_cancel_clone.clone())
        .await?;
      serve_http_on::<HTTP>(
        conn,
        &listen_properties_clone,
        lifetime.clone(),
        tx.clone(),
        options,
      );
    }
    #[allow(unreachable_code)]
    Ok::<_, HttpNextError>(())
  });

  // Set the handle after we start the future
  *RcRef::map(&resource, |this| &this.join_handle)
    .try_borrow_mut()
    .unwrap() = Some(handle);

  Ok((
    state.borrow_mut().resource_table.add_rc(resource),
    listen_properties.scheme,
    listen_properties.fallback_host,
    options.no_legacy_abort,
  ))
}

#[op2]
#[serde]
pub fn op_http_serve_on<HTTP>(
  state: Rc<RefCell<OpState>>,
  #[smi] connection_rid: ResourceId,
) -> Result<(ResourceId, &'static str, String, bool), HttpNextError>
where
  HTTP: HttpPropertyExtractor,
{
  let connection =
    HTTP::get_connection_for_rid(&mut state.borrow_mut(), connection_rid)?;

  let listen_properties = HTTP::listen_properties_from_connection(&connection)?;

  let (tx, rx) = tokio::sync::mpsc::channel(10);
  let resource: Rc<HttpJoinHandle> = Rc::new(HttpJoinHandle::new(rx));

  let options = {
    let state = state.borrow();
    *state.borrow::<Options>()
  };

  let handle = serve_http_on::<HTTP>(
    connection,
    &listen_properties,
    resource.lifetime(),
    tx,
    options,
  );

  // Set the handle after we start the future
  *RcRef::map(&resource, |this| &this.join_handle)
    .try_borrow_mut()
    .unwrap() = Some(handle);

  Ok((
    state.borrow_mut().resource_table.add_rc(resource),
    listen_properties.scheme,
    listen_properties.fallback_host,
    options.no_legacy_abort,
  ))
}

/// Synchronous, non-blocking call to see if there are any further HTTP requests. If anything
/// goes wrong in this method we return null and let the async handler pick up the real error.
#[op2(fast)]
pub fn op_http_try_wait(
  state: &mut OpState,
  #[smi] rid: ResourceId,
) -> *const c_void {
  // The resource needs to exist.
  let Ok(join_handle) = state.resource_table.get::<HttpJoinHandle>(rid) else {
    return null();
  };

  // If join handle is somehow locked, just abort.
  let Some(mut handle) =
    RcRef::map(&join_handle, |this| &this.rx).try_borrow_mut()
  else {
    return null();
  };

  // See if there are any requests waiting on this channel. If not, return.
  let Ok(record) = handle.try_recv() else {
    return null();
  };

  let ptr = ExternalPointer::new(RcHttpRecord(record));
  ptr.into_raw()
}

#[op2(async)]
pub async fn op_http_wait(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<*const c_void, HttpNextError> {
  // We will get the join handle initially, as we might be consuming requests still
  let join_handle = state
    .borrow_mut()
    .resource_table
    .get::<HttpJoinHandle>(rid)?;

  let cancel = join_handle.listen_cancel_handle();
  let next = async {
    let mut recv = RcRef::map(&join_handle, |this| &this.rx).borrow_mut().await;
    recv.recv().await
  }
  .or_cancel(cancel)
  .unwrap_or_else(|_| None)
  .await;

  // Do we have a request?
  if let Some(record) = next {
    let ptr = ExternalPointer::new(RcHttpRecord(record));
    return Ok(ptr.into_raw());
  }

  // No - we're shutting down
  let res = RcRef::map(join_handle, |this| &this.join_handle)
    .borrow_mut()
    .await
    .take()
    .unwrap()
    .await?;

  // Filter out shutdown (ENOTCONN) errors
  if let Err(err) = res {
    if let HttpNextError::Io(err) = &err {
      if err.kind() == io::ErrorKind::NotConnected {
        return Ok(null());
      }
    }

    if let HttpNextError::Other(err) = &err {
      if let Some(err) = err.as_any().downcast_ref::<std::io::Error>() {
        if let Some(err) = err.get_ref() {
          if let Some(err) =
            err.downcast_ref::<deno_net::tunnel::quinn::ConnectionError>()
          {
            if matches!(
              err,
              deno_net::tunnel::quinn::ConnectionError::LocallyClosed
            ) {
              return Ok(null());
            }
          }
        }
      }
    }

    return Err(err);
  }

  Ok(null())
}

/// Cancels the HTTP handle.
#[op2(fast)]
pub fn op_http_cancel(
  state: &mut OpState,
  #[smi] rid: ResourceId,
  graceful: bool,
) -> Result<(), deno_core::error::ResourceError> {
  let join_handle = state.resource_table.get::<HttpJoinHandle>(rid)?;

  if graceful {
    // In a graceful shutdown, we close the listener and allow all the remaining connections to drain
    join_handle.listen_cancel_handle().cancel();
  } else {
    // In a forceful shutdown, we close everything
    join_handle.listen_cancel_handle().cancel();
    join_handle.connection_cancel_handle().cancel();
  }

  Ok(())
}

#[op2(async)]
pub async fn op_http_close(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  graceful: bool,
) -> Result<(), HttpNextError> {
  let join_handle = state
    .borrow_mut()
    .resource_table
    .take::<HttpJoinHandle>(rid)?;

  if graceful {
    http_general_trace!("graceful shutdown");
    // In a graceful shutdown, we close the listener and allow all the remaining connections to drain
    join_handle.listen_cancel_handle().cancel();
    poll_fn(|cx| join_handle.server_state.poll_complete(cx)).await;
  } else {
    http_general_trace!("forceful shutdown");
    // In a forceful shutdown, we close everything
    join_handle.listen_cancel_handle().cancel();
    join_handle.connection_cancel_handle().cancel();
    // Give streaming responses a tick to close
    tokio::task::yield_now().await;
  }

  http_general_trace!("awaiting shutdown");

  let mut join_handle = RcRef::map(&join_handle, |this| &this.join_handle)
    .borrow_mut()
    .await;
  if let Some(join_handle) = join_handle.take() {
    join_handle.await??;
  }

  Ok(())
}

struct UpgradeStream {
  read: AsyncRefCell<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
  write: AsyncRefCell<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
  cancel_handle: CancelHandle,
}

impl UpgradeStream {
  pub fn new(
    read: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
  ) -> Self {
    Self {
      read: AsyncRefCell::new(read),
      write: AsyncRefCell::new(write),
      cancel_handle: CancelHandle::new(),
    }
  }

  async fn read(
    self: Rc<Self>,
    buf: &mut [u8],
  ) -> Result<usize, std::io::Error> {
    let cancel_handle = RcRef::map(self.clone(), |this| &this.cancel_handle);
    async {
      let read = RcRef::map(self, |this| &this.read);
      let mut read = read.borrow_mut().await;
      Pin::new(&mut *read).read(buf).await
    }
    .try_or_cancel(cancel_handle)
    .await
  }

  async fn write(self: Rc<Self>, buf: &[u8]) -> Result<usize, std::io::Error> {
    let cancel_handle = RcRef::map(self.clone(), |this| &this.cancel_handle);
    async {
      let write = RcRef::map(self, |this| &this.write);
      let mut write = write.borrow_mut().await;
      Pin::new(&mut *write).write(buf).await
    }
    .try_or_cancel(cancel_handle)
    .await
  }

  async fn write_vectored(
    self: Rc<Self>,
    buf1: &[u8],
    buf2: &[u8],
  ) -> Result<usize, std::io::Error> {
    let mut wr = RcRef::map(self, |r| &r.write).borrow_mut().await;

    let total = buf1.len() + buf2.len();
    let mut bufs = [std::io::IoSlice::new(buf1), std::io::IoSlice::new(buf2)];
    let mut nwritten = wr.write_vectored(&bufs).await?;
    if nwritten == total {
      return Ok(nwritten);
    }

    // Slightly more optimized than (unstable) write_all_vectored for 2 iovecs.
    while nwritten <= buf1.len() {
      bufs[0] = std::io::IoSlice::new(&buf1[nwritten..]);
      nwritten += wr.write_vectored(&bufs).await?;
    }

    // First buffer out of the way.
    if nwritten < total && nwritten > buf1.len() {
      wr.write_all(&buf2[nwritten - buf1.len()..]).await?;
    }

    Ok(total)
  }
}

impl Resource for UpgradeStream {
  fn name(&self) -> Cow<str> {
    "httpRawUpgradeStream".into()
  }

  deno_core::impl_readable_byob!();
  deno_core::impl_writable!();

  fn close(self: Rc<Self>) {
    self.cancel_handle.cancel();
  }
}

#[op2(fast)]
pub fn op_can_write_vectored(
  state: &mut OpState,
  #[smi] rid: ResourceId,
) -> bool {
  state.resource_table.get::<UpgradeStream>(rid).is_ok()
}

#[op2(async)]
#[number]
pub async fn op_raw_write_vectored(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  #[buffer] buf1: JsBuffer,
  #[buffer] buf2: JsBuffer,
) -> Result<usize, HttpNextError> {
  let resource: Rc<UpgradeStream> =
    state.borrow().resource_table.get::<UpgradeStream>(rid)?;
  let nwritten = resource.write_vectored(&buf1, &buf2).await?;
  Ok(nwritten)
}

#[op2(fast)]
pub fn op_http_metric_handle_otel_error(external: *const c_void) {
  let http =
    // SAFETY: external is deleted before calling this op.
    unsafe { take_external!(external, "op_http_metric_handle_otel_error") };

  http.otel_info_set_error("user");
}
macro_rules! lazy_strings {
  ($name: ident { $($field: ident = $value: expr),* }) => {
    #[derive(Clone)]
    struct $name {
      $(
        $field: v8::Global<v8::String>,
      )*
    }
    impl $name {
      fn new(scope: &mut v8::HandleScope) -> Self {
        $(
          let $field = v8::String::new_from_utf8(scope, $value.as_bytes(), v8::NewStringType::Internalized).unwrap();
          let $field = v8::Global::new(scope, $field);
        )*
        Self {
          $(
            $field,
          )*
        }
      }

      fn get_or_init(scope: &mut v8::HandleScope, op_state: &mut deno_core::OpState) -> Self {
        if let Some(strings) = op_state.try_borrow::<Self>() {
          strings.clone()
        } else {
          let strings = Self::new(scope);
          op_state.put(strings.clone());
          strings
        }
      }

      fn to_local<'scope>(string: &v8::Global<v8::String>, scope: &mut v8::HandleScope<'scope>) -> v8::Local<'scope, v8::String> {
        v8::Local::new(scope, string)
      }
    }

  }
}
v8_static_strings!(
  HEADER_LIST = "headerList",
  STATUS = "status",
  STATUS_MESSAGE = "statusMessage",
  TYPE = "type",
  URL = "url",
  URL_LIST = "urlList",
  BODY = "body",
  STREAM_OR_STATIC = "streamOrStatic",
);

lazy_strings!(Strings {
  header_list = "headerList",
  status = "status",
  status_message = "statusMessage",
  type_ = "type",
  url = "url",
  url_list = "urlList",
  body = "body",
  stream_or_static = "streamOrStatic"
});

#[op2(fast, reentrant)]
pub fn op_serve2(
  state: &mut OpState,
  scope: &mut v8::HandleScope,
  arg1: v8::Local<v8::Value>,
  arg2: v8::Local<v8::Value>,
) {
  let strings = Strings::get_or_init(scope, state);
  state.external_ops_tracker.ref_op();
  serve(scope, strings, arg1, arg2);
}

fn serve(
  scope: &mut v8::HandleScope,
  strings: Strings,
  arg1: v8::Local<v8::Value>,
  arg2: v8::Local<v8::Value>,
) {
  let isolate: &mut v8::Isolate = scope.as_mut();
  let isolate = isolate as *mut v8::Isolate;
  let mut options: Option<v8::Local<v8::Object>> = None;
  let mut handler: Option<v8::Local<v8::Function>> = None;

  if arg1.is_function() {
    handler = Some(arg1.cast::<v8::Function>());
  } else if arg2.is_function() {
    handler = Some(arg2.cast::<v8::Function>());
    options = Some(arg2.cast::<v8::Object>());
  } else {
    options = Some(arg1.cast::<v8::Object>());
  }
  let load_balanced = false;

  let addr = resolve_addr_sync("0.0.0.0", 8000).unwrap().next().unwrap();

  let listener = if load_balanced {
    TcpListener::bind_load_balanced(addr)
  } else {
    TcpListener::bind_direct(addr, false)
  }
  .unwrap();
  let local_addr = listener.local_addr().unwrap();
  let context = scope.get_current_context();
  let context = v8::Global::new(scope, context);
  let handler = v8::Global::new(scope, handler.unwrap());
  serve_http_on_listener(listener, strings, handler, isolate, context);
}

fn serve_http_on_listener(
  listener: TcpListener,
  strings: Strings,
  handler: v8::Global<v8::Function>,
  isolate: *mut v8::Isolate,
  context: v8::Global<v8::Context>,
) -> JoinHandle<()> {
  spawn(async move {
    loop {
      let (stream, addr) = listener.accept().await.unwrap();
      let context = context.clone();
      let handler = handler.clone();
      let strings = strings.clone();
      let svc = service_fn(move |req: Request| {
        let context = context.clone();
        let handler = handler.clone();
        let strings = strings.clone();
        async move {
          let scope = &mut v8::HandleScope::new(unsafe { &mut *isolate });
          let context = v8::Local::new(scope, context);
          let scope = &mut v8::ContextScope::new(scope, context);
          let recv = v8::null(scope).into();
          let handler = v8::Local::new(scope, handler);
          let response = handler.call(scope, recv, &[]).unwrap();
          let global = v8::Global::new(scope, response);
          let response = deno_core::JsRuntime::scoped_resolve(scope, global)
            .await
            .unwrap();
          let response = v8::Local::new(scope, response);
          Ok::<_, HttpNextError>(response_from_value(scope, &strings, response))
        }
      });
      let connection_cancel_handle = CancelHandle::new_rc();
      spawn(async move {
        serve_http2_autodetect(
          stream,
          svc,
          CancelHandle::new_rc(),
          Options {
            http2_builder_hook: None,
            http1_builder_hook: None,
            no_legacy_abort: true,
          },
        )
        .try_or_cancel(connection_cancel_handle)
        .await
        .unwrap();
      });
    }
  })
}

fn response_from_value(
  scope: &mut v8::HandleScope,
  strings: &Strings,
  value: v8::Local<v8::Value>,
) -> hyper::Response<Resp> {
  let value = value.cast::<v8::Object>();

  macro_rules! get {
    ($value: ident . $name: ident) => {{
      let name = Strings::to_local(&strings.$name, scope);
      $value.get(scope, name.into())
    }};
    ($name: ident) => {{
      let name = Strings::to_local(&strings.$name, scope);
      value.get(scope, name.into())
    }};
  }
  let header_list = get!(header_list);
  let status = get!(status);
  let status_message = get!(status_message);
  let type_ = get!(type_);
  let url = get!(url);
  let url_list = get!(url_list);
  let body = get!(body).unwrap();

  let mut headers = HeaderMap::with_capacity(10);
  if let Some(header_list) = header_list {
    // [string,string][]
    let header_list = header_list.cast::<v8::Array>();
    for i in 0..header_list.length() {
      let item = header_list.get_index(scope, i).unwrap();
      let item = item.cast::<v8::Array>();
      let name = item.get_index(scope, 0).unwrap().cast::<v8::String>();
      let value = item.get_index(scope, 1).unwrap().cast::<v8::String>();
      headers.insert(
        http::HeaderName::from_bytes(
          name.to_rust_string_lossy(scope).as_bytes(),
        )
        .unwrap(),
        http::HeaderValue::from_bytes(
          value.to_rust_string_lossy(scope).as_bytes(),
        )
        .unwrap(),
      );
    }
  }
  headers.insert(
    HeaderName::from_static("vary"),
    HeaderValue::from_static("accept-encoding"),
  );

  let status = status.unwrap().cast::<v8::Number>().value();
  let (mut parts, _) = http::Response::new(()).into_parts();
  parts.headers = headers;
  parts.status = StatusCode::from_u16(status as u16).unwrap();
  if body.is_null_or_undefined() {
    return hyper::Response::from_parts(parts, Resp(RefCell::new(None)));
  }
  let body_obj = body.cast::<v8::Object>();
  let stream_or_static = get!(body_obj.stream_or_static).unwrap();
  if stream_or_static.is_null_or_undefined() {
    return hyper::Response::from_parts(parts, Resp(RefCell::new(None)));
  } else {
    let stream_or_static = stream_or_static.cast::<v8::Object>();
    let body = get!(stream_or_static.body).unwrap();
    if body.is_string() {
      let body = body.cast::<v8::String>().to_rust_string_lossy(scope);
      hyper::Response::from_parts(
        parts,
        Resp(RefCell::new(Some(RespInner {
          trailers: None,
          response_body: ResponseBytesInner::Bytes(body.into_bytes().into()),
        }))),
      )
    } else {
      todo!()
    }
  }
}

// `None` variant used when no body is present, for example
// when we want to return a synthetic 400 for invalid requests.
pub struct Resp(pub(crate) RefCell<Option<RespInner>>);

pub struct RespInner {
  pub trailers: Option<HeaderMap>,
  pub response_body: ResponseBytesInner,
}

impl Body for Resp {
  type Data = BufView;
  type Error = JsErrorBox;

  fn poll_frame(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
    use crate::response_body::PollFrame;
    let mut thing = self.0.borrow_mut();
    let Some(record) = thing.as_mut() else {
      return Poll::Ready(None);
    };

    let res = loop {
      let res = match &mut record.response_body {
        ResponseBytesInner::Done | ResponseBytesInner::Empty => {
          if let Some(trailers) = record.trailers.take() {
            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
          }
          unreachable!()
        }
        ResponseBytesInner::Bytes(..) => {
          let bytes = std::mem::replace(
            &mut record.response_body,
            ResponseBytesInner::Done,
          );
          let ResponseBytesInner::Bytes(data) = bytes else {
            unreachable!();
          };
          return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        ResponseBytesInner::UncompressedStream(stm) => {
          ready!(Pin::new(stm).poll_frame(cx))
        }
        ResponseBytesInner::GZipStream(stm) => {
          ready!(Pin::new(stm.as_mut()).poll_frame(cx))
        }
        ResponseBytesInner::BrotliStream(stm) => {
          ready!(Pin::new(stm.as_mut()).poll_frame(cx))
        }
      };
      // This is where we retry the NoData response
      if matches!(res, ResponseStreamResult::NoData) {
        continue;
      }
      break res;
    };

    if matches!(res, ResponseStreamResult::EndOfStream) {
      if let Some(trailers) = record.trailers.take() {
        return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
      }
      record.response_body = ResponseBytesInner::Done;
    }

    // if let ResponseStreamResult::NonEmptyBuf(buf) = &res {
    //   let mut http = record.0.borrow_mut();
    //   if let Some(otel_info) = &mut http.as_mut().unwrap().otel_info {
    //     if let Some(response_size) = &mut otel_info.response_size {
    //       *response_size += buf.len() as u64;
    //     }
    //   }
    // }

    Poll::Ready(res.into())
  }

  fn is_end_stream(&self) -> bool {
    let mut thing = self.0.borrow();
    let Some(record) = thing.as_ref() else {
      return true;
    };
    matches!(
      record.response_body,
      ResponseBytesInner::Done | ResponseBytesInner::Empty
    ) && record.trailers.is_none()
  }

  fn size_hint(&self) -> SizeHint {
    let mut thing = self.0.borrow();
    let Some(record) = thing.as_ref() else {
      return SizeHint::with_exact(0);
    };
    // The size hint currently only used in the case where it is exact bounds in hyper, but we'll pass it through
    // anyways just in case hyper needs it.
    record.response_body.size_hint()
  }
}

impl Drop for Resp {
  fn drop(&mut self) {
    let mut thing = self.0.borrow_mut();
    let Some(record) = thing.as_mut() else {
      return;
    };
    // SAFETY: this ManuallyDrop is not used again.
    // let record = unsafe { ManuallyDrop::take(record) };
    // http_trace!(record, "HttpRecordResponse::drop");
    // record.finish();
  }
}

struct Context {}

// fn make_callback(
//   handler: v8::Local<v8::Function>,
// ) -> impl FnOnce(Request) -> Response {
// }
