// Copyright 2018-2025 the Deno authors. MIT license.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use deno_core::error::AnyError;
use deno_terminal::colors;
use fastwebsockets::WebSocket;
use hyper::body::Incoming as IncomingBody;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, broadcast};
// TokioIo already imported above

use super::BundlerInput;
use super::EsbuildBundler;
use super::OutputFile;
use super::html;
use super::maybe_process_contents;
use crate::args::Flags;
use deno_bundle_runtime::BundlePlatform;
use esbuild_client::protocol::BuildResponse;
// no Rc/RefCell needed now; watch flow handled in mod.rs

const OUT_BASE: &str = "/@out";

#[derive(Clone)]
pub struct DevAsset {
  bytes: Arc<Vec<u8>>,
  content_type: String,
  etag: String,
}

#[derive(Clone)]
pub struct DevAssetStore {
  // request path -> asset
  map: Arc<RwLock<HashMap<String, DevAsset>>>,
  // path to serve for '/'
  default_html: Arc<RwLock<Option<String>>>,
  // cwd for disk fallback
  cwd: Arc<PathBuf>,
  // broadcaster for live reload
  btx: broadcast::Sender<Bytes>,
}

impl DevAssetStore {
  fn new(cwd: PathBuf, btx: broadcast::Sender<Bytes>) -> Self {
    Self {
      map: Default::default(),
      default_html: Default::default(),
      cwd: Arc::new(cwd),
      btx,
    }
  }

  async fn insert(&self, path: String, bytes: Vec<u8>, content_type: String) {
    let etag = hex_sha256(&bytes);
    let asset = DevAsset {
      bytes: Arc::new(bytes),
      content_type,
      etag,
    };
    self.map.write().await.insert(path, asset);
  }

  async fn get(&self, path: &str) -> Option<DevAsset> {
    self.map.read().await.get(path).cloned()
  }

  async fn set_default_html(&self, path: String) {
    *self.default_html.write().await = Some(path);
  }
  async fn get_default_html(&self) -> Option<String> {
    self.default_html.read().await.clone()
  }

  fn broadcast_reload(&self) {
    let _ = self.btx.send(Bytes::from_static(br#"{"type":"reload"}"#));
  }
}

#[derive(Clone)]
pub struct DevServerController {
  store: Arc<DevAssetStore>,
  is_dev: bool,
}

impl DevServerController {
  pub fn new(store: Arc<DevAssetStore>, is_dev: bool) -> Self {
    Self { store, is_dev }
  }

  pub async fn apply_response(
    &self,
    response: &BuildResponse,
    cwd: &Path,
    input: BundlerInput,
    platform: BundlePlatform,
    minified: bool,
  ) -> Result<(), AnyError> {
    let files = collect_output_files_for_serve(
      response.output_files.as_deref(),
      cwd,
      input,
      self.is_dev,
    )?;

    let mut first_html: Option<String> = None;
    for file in files.iter() {
      let processed = maybe_process_contents(
        file,
        super::should_replace_require_shim(platform),
        minified,
      )?;
      let path = &file.path;
      let content_type = guess_content_type(path).to_string();
      let contents =
        processed.contents.unwrap_or_else(|| file.contents.to_vec());
      let req_path = to_req_path(path);
      if first_html.is_none() && path.extension().is_some_and(|e| e == "html") {
        first_html = Some(req_path.clone());
      }
      self.store.insert(req_path, contents, content_type).await;
    }
    if let Some(html) = first_html {
      self.store.set_default_html(html).await;
    }
    self.store.broadcast_reload();
    Ok(())
  }
}

fn hex_sha256(data: &[u8]) -> String {
  let mut hasher = Sha256::new();
  hasher.update(data);
  let digest = hasher.finalize();
  faster_hex::hex_string(digest.as_slice())
}

fn guess_content_type(path: &Path) -> &'static str {
  match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
    "html" => "text/html",
    "js" | "mjs" => "application/javascript",
    "ts" | "tsx" => "application/typescript",
    "css" => "text/css",
    "map" => "application/json",
    "svg" => "image/svg+xml",
    "png" => "image/png",
    "jpg" | "jpeg" => "image/jpeg",
    "gif" => "image/gif",
    "ico" => "image/x-icon",
    _ => "application/octet-stream",
  }
}

pub async fn bundle_serve(
  flags: Arc<Flags>,
  bundler: EsbuildBundler,
  response: BuildResponse,
  minified: bool,
  platform: BundlePlatform,
  addr: SocketAddr,
  watch: bool,
  is_dev: bool,
) -> Result<(), AnyError> {
  let cwd = bundler.cwd.clone();
  let (btx, _brx) = broadcast::channel::<Bytes>(16);
  let store = DevAssetStore::new(cwd.clone(), btx.clone());
  let store = Arc::new(store);
  let controller = DevServerController::new(store.clone(), is_dev);

  // Seed only for non-watch (initial build in watch mode is a placeholder)
  if !watch {
    controller
      .apply_response(
        &response,
        &cwd,
        bundler.input.clone(),
        platform,
        minified,
      )
      .await?;
  }

  // In watch mode, the first real outputs arrive via rebuild; we seed after that.

  // Start server
  let listener = TcpListener::bind(addr).await?;
  log::info!(
    "{} {}",
    colors::green("Serving"),
    colors::cyan(format!("http://{}", listener.local_addr()?))
  );

  if watch {
    let server_store = store.clone();
    tokio::spawn(async move {
      loop {
        match listener.accept().await {
          Ok((stream, _peer)) => {
            let io = TokioIo::new(stream);
            let server_store = server_store.clone();
            tokio::spawn(async move {
              let service = service_fn(move |req: Request<IncomingBody>| {
                let store = server_store.clone();
                async move { route_request(store, req).await }
              });
              if let Err(err) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
              {
                log::debug!("dev server connection error: {err:?}");
              }
            });
          }
          Err(err) => {
            log::debug!("accept error: {err:?}");
          }
        }
      }
    });

    // delegate to shared watch flow and apply to memory store
    super::bundle_watch(
      flags,
      bundler,
      minified,
      platform,
      None,
      Some(controller),
    )
    .await
  } else {
    // no watch: run accept loop in this task
    loop {
      let (stream, _peer) = listener.accept().await?;
      let io = TokioIo::new(stream);
      let store = store.clone();
      tokio::spawn(async move {
        let service = service_fn(move |req: Request<IncomingBody>| {
          let store = store.clone();
          async move { route_request(store, req).await }
        });
        if let Err(err) = http1::Builder::new()
          .serve_connection(io, service)
          .with_upgrades()
          .await
        {
          log::debug!("dev server connection error: {err:?}");
        }
      });
    }
  }
}

async fn route_request(
  store: Arc<DevAssetStore>,
  mut req: Request<IncomingBody>,
) -> http::Result<Response<Box<http_body_util::Full<Bytes>>>> {
  // WebSocket endpoint for live reload
  if req.method() == http::Method::GET && req.uri().path() == "/__hmr" {
    let (resp, fut) = match fastwebsockets::upgrade::upgrade(&mut req) {
      Ok((resp, fut)) => {
        let (parts, _body) = resp.into_parts();
        let resp = http::Response::from_parts(
          parts,
          Box::new(http_body_util::Full::new(Bytes::new())),
        );
        (resp, fut)
      }
      _ => {
        return http::Response::builder()
          .status(http::StatusCode::BAD_REQUEST)
          .body(Box::new(http_body_util::Full::new(Bytes::from_static(
            b"Not a valid Websocket Request",
          ))));
      }
    };

    tokio::spawn(async move {
      match fut.await {
        Ok(ws) => {
          if let Err(e) = pump_reload_ws(ws, store.btx.subscribe()).await {
            log::debug!("hmr ws closed: {e:?}");
          }
        }
        Err(err) => log::debug!("upgrade error: {err:?}"),
      }
    });

    return Ok(resp);
  }
  let path = req.uri().path().to_string();
  let path_to_try = path.clone();
  if path == "/" {
    if let Some(default_html) = store.get_default_html().await {
      // redirect to the in-memory html so relative paths resolve correctly
      return http::Response::builder()
        .status(http::StatusCode::FOUND)
        .header(http::header::LOCATION, default_html)
        .body(Box::new(http_body_util::Full::new(Bytes::new())));
    }
  }

  if let Some(asset) = store.get(&path_to_try).await {
    let mut res = Response::new(Box::new(http_body_util::Full::new(
      Bytes::from(asset.bytes.as_ref().clone()),
    )));
    let headers = res.headers_mut();
    headers.insert(
      http::header::CACHE_CONTROL,
      http::HeaderValue::from_static("no-store"),
    );
    headers.insert(
      http::header::CONTENT_TYPE,
      http::HeaderValue::from_str(&asset.content_type)
        .unwrap_or(http::HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
      http::header::ETAG,
      http::HeaderValue::from_str(&format!("\"{}\"", asset.etag)).unwrap(),
    );
    return Ok(res);
  }

  // Fallback to disk for non-bundled static files
  // Normalize path (basic safeguard; denies .. traversal)
  if let Some(resp) = try_serve_disk(&store, &path_to_try).await {
    return Ok(resp);
  }

  http::Response::builder()
    .status(http::StatusCode::NOT_FOUND)
    .body(Box::new(http_body_util::Full::new(Bytes::from_static(
      b"Not Found",
    ))))
}

async fn try_serve_disk(
  store: &DevAssetStore,
  request_path: &str,
) -> Option<Response<Box<http_body_util::Full<Bytes>>>> {
  let mut safe = PathBuf::new();
  for seg in request_path.split('/') {
    if seg.is_empty() || seg == "." {
      continue;
    }
    if seg == ".." {
      return None;
    }
    safe.push(seg);
  }
  let mut abs = store.cwd.as_ref().clone();
  abs.push(safe);
  match tokio::fs::read(&abs).await {
    Ok(bytes) => {
      let mut res =
        Response::new(Box::new(http_body_util::Full::new(Bytes::from(bytes))));
      let headers = res.headers_mut();
      headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
      );
      headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(guess_content_type(&abs)),
      );
      Some(res)
    }
    Err(_) => None,
  }
}

fn to_req_path(path: &Path) -> String {
  // ensure forward slashes in URLs
  let p = path_components_as_posix(path);
  format!("{OUT_BASE}/{}", p)
}

fn path_components_as_posix(path: &Path) -> String {
  let mut comps: Vec<String> = Vec::new();
  for c in path.components() {
    let s = c.as_os_str().to_string_lossy().to_string();
    comps.push(s);
  }
  comps.join("/")
}

async fn pump_reload_ws(
  mut websocket: WebSocket<TokioIo<hyper::upgrade::Upgraded>>,
  mut rx: broadcast::Receiver<Bytes>,
) -> Result<(), AnyError> {
  // greet client
  let hello: &[u8] = br#"{"type":"connected","protocol":1}"#;
  websocket
    .write_frame(fastwebsockets::Frame::text(hello.into()))
    .await?;

  loop {
    tokio::select! {
      Ok(msg) = rx.recv() => {
        websocket.write_frame(fastwebsockets::Frame::text(msg.to_vec().into())).await?;
      }
      frame = websocket.read_frame() => {
        match frame {
          Ok(f) => {
            match f.opcode {
              fastwebsockets::OpCode::Close => break,
              fastwebsockets::OpCode::Ping => {
                websocket.write_frame(fastwebsockets::Frame::pong(vec![].into())).await?;
              }
              _ => {}
            }
          }
          Err(_) => break,
        }
      }
    }
  }
  Ok(())
}

// (watch logic is reused from bundle_watch in mod.rs)

pub fn collect_output_files_for_serve<'a>(
  response_output_files: Option<
    &'a [esbuild_client::protocol::BuildOutputFile],
  >,
  cwd: &Path,
  input: BundlerInput,
  is_dev: bool,
) -> Result<Vec<OutputFile<'a>>, AnyError> {
  // 1. Start with owned OutputFile entries
  let mut output_files: Vec<OutputFile<'a>> = response_output_files
    .map(|fs| fs.iter().map(|f| f.clone().into()).collect())
    .unwrap_or_default();

  // 2. Remap paths to a memory prefix and make them relative to cwd when possible
  for f in output_files.iter_mut() {
    let p = &f.path;
    let rel = pathdiff::diff_paths(p, cwd).unwrap_or_else(|| p.to_path_buf());
    let req = Path::new(OUT_BASE).join(rel);
    f.path = req;
  }

  // 3. If there are HTML entrypoints, patch and add HTML outputs using html.rs
  if let BundlerInput::EntrypointsWithHtml {
    entries: _,
    html_pages,
  } = input
  {
    let outdir = Path::new(OUT_BASE);
    let mut html_output_files = html::HtmlOutputFiles::new(&mut output_files);
    for page in html_pages {
      page.patch_html_with_response(cwd, outdir, &mut html_output_files)?;
    }
  }

  if is_dev {
    // inject a tiny live-reload client into any HTML outputs
    for file in output_files.iter_mut() {
      if file.path.extension().is_some_and(|e| e == "html") {
        let html = String::from_utf8(file.contents.clone().into_owned())?;
        let injected = inject_dev_client(&html);
        file.contents = std::borrow::Cow::Owned(injected.into_bytes());
      }
    }
  }

  Ok(output_files)
}

fn inject_dev_client(input: &str) -> String {
  let client = r#"<script type="module">
(() => {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const url = `${proto}://${location.host}/__hmr`;
  try {
    const ws = new WebSocket(url);
    ws.onmessage = (ev) => { try { const m = JSON.parse(ev.data); if (m?.type === 'reload') location.reload(); } catch {}
    };
  } catch {}
})();
</script>
"#;
  if let Some(idx) = input.find("</head>") {
    let mut out = String::with_capacity(input.len() + client.len());
    out.push_str(&input[..idx]);
    out.push_str(client);
    out.push_str(&input[idx..]);
    out
  } else {
    let mut out = String::with_capacity(input.len() + client.len());
    out.push_str(input);
    out.push_str(client);
    out
  }
}
