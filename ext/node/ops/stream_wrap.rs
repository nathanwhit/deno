// Copyright 2018-2026 the Deno authors. MIT license.

use std::cell::Cell;
use std::cell::RefCell;
use std::ops::DerefMut;
#[cfg(unix)]
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;

use std::num::NonZeroUsize;
use std::sync::Arc;

use deno_core::AsyncRefCell;
use deno_core::BufMutView;
use deno_core::BufView;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::CppgcBase;
use deno_core::CppgcInherits;
use deno_core::GarbageCollected;
use deno_core::OpState;
use deno_core::Resource;
use deno_core::op2;
use deno_core::v8;
use deno_net::DefaultTlsOptions;
use deno_net::UnsafelyIgnoreCertificateErrors;
use deno_tls::SocketUse;
use deno_tls::TlsClientConfigOptions;
use deno_tls::TlsKeys;
use deno_tls::create_client_config;
use deno_tls::rustls::ClientConnection;
use deno_tls::rustls::pki_types::CertificateDer;
use deno_tls::rustls::pki_types::ServerName;
use rustls_tokio_stream::TlsStream;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;

use super::handle_wrap::AsyncWrap;
use super::handle_wrap::HandleWrap;

const TLS_BUFFER_SIZE: Option<NonZeroUsize> = NonZeroUsize::new(65536);

// UV error codes. EOF and UNKNOWN are libuv-internal constants (same on all
// platforms). For system errors on Unix the UV code is simply `-errno`, so we
// derive those at runtime via `raw_os_error()`.
pub(crate) const UV_EOF: i32 = -4095;
pub(crate) const UV_UNKNOWN: i32 = -4094;
pub(crate) const UV_EBADF: i32 = -(libc::EBADF as i32);

const SUGGESTED_SIZE: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Stream backend enums – the Rust equivalent of uv_stream_t
// ---------------------------------------------------------------------------

pub(crate) enum ReadHalf {
  Tcp(tokio::net::tcp::OwnedReadHalf),
  #[cfg(unix)]
  Pipe(tokio::net::unix::OwnedReadHalf),
  Tls(rustls_tokio_stream::TlsStreamRead<tokio::net::TcpStream>),
}

pub(crate) enum WriteHalf {
  Tcp(tokio::net::tcp::OwnedWriteHalf),
  #[cfg(unix)]
  Pipe(tokio::net::unix::OwnedWriteHalf),
  Tls(rustls_tokio_stream::TlsStreamWrite<tokio::net::TcpStream>),
}

impl AsyncRead for ReadHalf {
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
  ) -> Poll<std::io::Result<()>> {
    match self.get_mut() {
      ReadHalf::Tcp(r) => Pin::new(r).poll_read(cx, buf),
      #[cfg(unix)]
      ReadHalf::Pipe(r) => Pin::new(r).poll_read(cx, buf),
      ReadHalf::Tls(r) => Pin::new(r).poll_read(cx, buf),
    }
  }
}

impl AsyncWrite for WriteHalf {
  fn poll_write(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<std::io::Result<usize>> {
    match self.get_mut() {
      WriteHalf::Tcp(w) => Pin::new(w).poll_write(cx, buf),
      #[cfg(unix)]
      WriteHalf::Pipe(w) => Pin::new(w).poll_write(cx, buf),
      WriteHalf::Tls(w) => Pin::new(w).poll_write(cx, buf),
    }
  }

  fn poll_flush(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<std::io::Result<()>> {
    match self.get_mut() {
      WriteHalf::Tcp(w) => Pin::new(w).poll_flush(cx),
      #[cfg(unix)]
      WriteHalf::Pipe(w) => Pin::new(w).poll_flush(cx),
      WriteHalf::Tls(w) => Pin::new(w).poll_flush(cx),
    }
  }

  fn poll_shutdown(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<std::io::Result<()>> {
    match self.get_mut() {
      WriteHalf::Tcp(w) => Pin::new(w).poll_shutdown(cx),
      #[cfg(unix)]
      WriteHalf::Pipe(w) => Pin::new(w).poll_shutdown(cx),
      WriteHalf::Tls(w) => Pin::new(w).poll_shutdown(cx),
    }
  }
}

// ---------------------------------------------------------------------------
// Shared inner state – lives in Rc so async tasks can reference it
// ---------------------------------------------------------------------------

pub(crate) struct LibuvStreamInner {
  // Wrapped in Rc so that AsyncRefCell::borrow_mut / try_borrow_mut
  // are available (they require Rc<AsyncRefCell<T>>).
  read: Rc<AsyncRefCell<Option<ReadHalf>>>,
  write: Rc<AsyncRefCell<Option<WriteHalf>>>,
  /// Fallback: generic Deno resource for handles not yet ported to
  /// native tokio halves (e.g. child-process stdio pipes).
  resource: RefCell<Option<Rc<dyn Resource>>>,
  /// Remember the rid so we can re-acquire the resource after readStop
  /// drops it (needed for HTTP keepAlive reuse, etc.).
  resource_rid: Cell<Option<u32>>,
  /// Raw OS file descriptor for the underlying socket, captured at
  /// attachment time so socket options (nodelay, keepalive) can be set
  /// even after the stream is split.
  #[cfg(unix)]
  raw_fd: Cell<Option<RawFd>>,
  cancel: RefCell<Rc<CancelHandle>>,
  /// Set to `true` while `upgradeTls` has taken the TCP halves and the TLS
  /// handshake is in progress.  `writeBuffer` / `shutdown` check this flag
  /// instead of returning UV_EBADF and wait for the new TLS halves.
  tls_upgrading: Cell<bool>,
  tls_upgrade_notify: tokio::sync::Notify,
  /// Whether the consumer wants to be reading.
  reading: Cell<bool>,
  /// Whether a read-loop task is currently spawned.
  read_active: Cell<bool>,
  bytes_read: Cell<u64>,
  bytes_written: Cell<u64>,
  /// Handle back to the JS object (for calling `onread`).
  this: v8::Global<v8::Object>,
  spawner: deno_core::V8TaskSpawner,
  /// Keeps the event loop alive while a read loop is active.
  read_ref_tracker: super::RefTracker,
  /// Whether the user has called unref() on this handle. When set,
  /// starting a read loop will NOT ref the event loop. This mirrors
  /// libuv's behavior where uv_unref(handle) prevents active I/O
  /// from keeping the event loop alive.
  user_unrefed: Cell<bool>,
  /// Peer certificates from TLS handshake (set by upgradeTls).
  peer_certificates:
    RefCell<Option<Vec<CertificateDer<'static>>>>,
}

// ---------------------------------------------------------------------------
// Read loop
// ---------------------------------------------------------------------------

static ONREAD_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("onread");

fn ref_read_loop(inner: &LibuvStreamInner) {
  // Don't ref the event loop if the user has unref'd this handle.
  // This mirrors libuv where uv_unref(handle) prevents active I/O
  // from keeping the event loop alive.
  if !inner.user_unrefed.get() {
    inner.read_ref_tracker.ref_();
  }
}

fn unref_read_loop(inner: &LibuvStreamInner) {
  inner.read_ref_tracker.unref();
}

fn start_read_loop(inner: &Rc<LibuvStreamInner>) {
  inner.reading.set(true);
  if inner.read_active.get() {
    // An existing read-loop task is alive – it will check `reading` and
    // continue on its next iteration.
    return;
  }
  inner.read_active.set(true);

  // Ref the event loop BEFORE spawning the async task. This prevents
  // a race where the event loop might exit after connect unrefs but
  // before the async task is polled (which is where ref used to happen).
  ref_read_loop(inner);

  let inner = inner.clone();
  deno_core::unsync::spawn(async move {
    read_loop(&inner).await;
    unref_read_loop(&inner);

    inner.read_active.set(false);
    // If readStart() was called while we were winding down, re-enter.
    if inner.reading.get() {
      start_read_loop(&inner);
    }
  });
}

async fn read_loop(inner: &Rc<LibuvStreamInner>) {
  let mut read_guard = inner.read.borrow_mut().await;
  let has_halves = read_guard.is_some();

  if has_halves {
    read_loop_tokio(inner, &mut read_guard).await;
  } else {
    // No tokio halves – try the generic Resource fallback.
    drop(read_guard);
    read_loop_resource(inner).await;
  }
}

/// Read loop for native tokio stream halves (TCP, Unix socket, …).
async fn read_loop_tokio(
  inner: &Rc<LibuvStreamInner>,
  read_guard: &mut Option<ReadHalf>,
) {
  let read: &mut ReadHalf = match read_guard.as_mut() {
    Some(r) => r,
    None => return,
  };

  let mut buf = vec![0u8; SUGGESTED_SIZE];

  loop {
    if !inner.reading.get() {
      break;
    }

    let cancel = inner.cancel.borrow().clone();
    let result = read.read(&mut buf).or_cancel(cancel).await;

    let nread: i32 = match result {
      Ok(Ok(0)) => UV_EOF,
      Ok(Ok(n)) => n as i32,
      Ok(Err(e)) => io_error_to_uv_code(&e),
      Err(_cancelled) => break,
    };

    if dispatch_onread(inner, &buf, nread).await {
      break;
    }
  }
}

/// Read loop using a generic `deno_core::Resource` (child-process stdio, etc.).
/// Re-acquires the resource Rc from `inner.resource` each iteration so that
/// `detachResource()` can drop the refcount without racing the async task.
async fn read_loop_resource(inner: &Rc<LibuvStreamInner>) {
  loop {
    if !inner.reading.get() {
      break;
    }

    // Clone the Rc just for this iteration. If detachResource() was called,
    // inner.resource will be None and we exit.
    let resource = inner.resource.borrow().clone();
    let Some(resource) = resource else {
      // Nothing to read from – stop so the re-entry check in
      // start_read_loop doesn't spin forever.
      inner.reading.set(false);
      break;
    };

    let cancel = inner.cancel.borrow().clone();
    let buf = BufMutView::new(SUGGESTED_SIZE);
    let result = resource.clone().read_byob(buf).or_cancel(cancel).await;
    // Drop the resource Rc immediately so it's not held across dispatching.
    drop(resource);

    let (nread, data): (i32, Vec<u8>) = match result {
      Ok(Ok((0, _buf))) => (UV_EOF, Vec::new()),
      Ok(Ok((n, buf))) => (n as i32, buf[..n].to_vec()),
      Ok(Err(_e)) => (UV_UNKNOWN, Vec::new()),
      Err(_cancelled) => break,
    };

    if dispatch_onread(inner, &data, nread).await {
      break;
    }
  }
}

/// Dispatch the `onread` callback and return `true` if the loop should stop.
async fn dispatch_onread(
  inner: &Rc<LibuvStreamInner>,
  data: &[u8],
  nread: i32,
) -> bool {
  if nread > 0 {
    inner.bytes_read.set(inner.bytes_read.get() + nread as u64);
  }

  let data = if nread > 0 {
    data[..nread as usize].to_vec()
  } else {
    Vec::new()
  };

  // Synchronise with the JS thread: wait for the callback to finish
  // before issuing the next read. This gives JS a chance to call
  // readStop() and provides natural back-pressure.
  let (tx, rx) = tokio::sync::oneshot::channel::<()>();
  let this_global = inner.this.clone();
  inner.spawner.spawn(move |scope| {
    let this = v8::Local::new(scope, &this_global);
    call_onread(scope, this, &data, nread);
    let _ = tx.send(());
  });
  let _ = rx.await;

  if nread <= 0 {
    // EOF or error – stop reading.
    inner.reading.set(false);
    return true;
  }
  false
}

fn call_onread(
  scope: &mut v8::PinScope<'_, '_>,
  this: v8::Local<v8::Object>,
  data: &[u8],
  nread: i32,
) {
  let onread_key = ONREAD_STR.v8_string(scope).unwrap();
  let Some(onread) = this.get(scope, onread_key.into()) else {
    return;
  };
  let Ok(onread_fn) = onread.try_cast::<v8::Function>() else {
    return;
  };

  let nread_val: v8::Local<v8::Value> = v8::Integer::new(scope, nread).into();

  let buf_val: v8::Local<v8::Value> = if !data.is_empty() {
    let store = v8::ArrayBuffer::new_backing_store_from_vec(data.to_vec());
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store.into());
    v8::Uint8Array::new(scope, ab, 0, data.len())
      .unwrap()
      .into()
  } else {
    v8::undefined(scope).into()
  };

  onread_fn.call(scope, this.into(), &[buf_val, nread_val]);
}

pub(crate) fn io_error_to_uv_code(e: &std::io::Error) -> i32 {
  if let Some(errno) = e.raw_os_error() {
    -errno
  } else {
    UV_UNKNOWN
  }
}

/// Write all data through a generic `deno_core::Resource`.
async fn write_all_resource(
  resource: &Rc<dyn Resource>,
  data: &[u8],
  inner: &LibuvStreamInner,
) -> i32 {
  let mut offset = 0usize;
  while offset < data.len() {
    let view = BufView::from(data[offset..].to_vec());
    match resource.clone().write(view).await {
      Ok(outcome) => {
        let n = outcome.nwritten();
        inner
          .bytes_written
          .set(inner.bytes_written.get() + n as u64);
        offset += n;
      }
      Err(_) => return UV_UNKNOWN,
    }
  }
  0
}

// ---------------------------------------------------------------------------
// Write helpers
// ---------------------------------------------------------------------------

/// Write data, waiting for a TLS upgrade if one is in progress instead of
/// returning UV_EBADF immediately.
async fn write_with_tls_retry(
  inner: &LibuvStreamInner,
  data: &[u8],
  resource_snapshot: Option<Rc<dyn Resource>>,
) -> i32 {
  let mut write_guard = inner.write.borrow_mut().await;
  if let Some(write) = write_guard.deref_mut() {
    return match write.write_all(data).await {
      Ok(()) => {
        inner
          .bytes_written
          .set(inner.bytes_written.get() + data.len() as u64);
        0
      }
      Err(e) => io_error_to_uv_code(&e),
    };
  }

  // Write half is None.  If a TLS upgrade is in progress, wait for it.
  if inner.tls_upgrading.get() {
    drop(write_guard);
    inner.tls_upgrade_notify.notified().await;
    // Re-acquire after TLS halves are stored.
    let mut write_guard = inner.write.borrow_mut().await;
    if let Some(write) = write_guard.deref_mut() {
      return match write.write_all(data).await {
        Ok(()) => {
          inner
            .bytes_written
            .set(inner.bytes_written.get() + data.len() as u64);
          0
        }
        Err(e) => io_error_to_uv_code(&e),
      };
    }
    return UV_EBADF;
  }

  // Fallback: generic Resource write.
  // Keep the write_guard held to serialize writes — Node.js processes
  // queued writes sequentially, so concurrent resource writes must not
  // interleave.
  if let Some(resource) = resource_snapshot {
    write_all_resource(&resource, data, inner).await
  } else {
    UV_EBADF
  }
}

/// Shutdown the write half, waiting for a TLS upgrade if one is in progress.
async fn shutdown_with_tls_retry(
  inner: &LibuvStreamInner,
  resource_snapshot: Option<Rc<dyn Resource>>,
) -> i32 {
  let mut write_guard = inner.write.borrow_mut().await;
  if let Some(write) = write_guard.deref_mut() {
    return match write.shutdown().await {
      Ok(()) => 0,
      Err(e) => io_error_to_uv_code(&e),
    };
  }

  if inner.tls_upgrading.get() {
    drop(write_guard);
    inner.tls_upgrade_notify.notified().await;
    let mut write_guard = inner.write.borrow_mut().await;
    if let Some(write) = write_guard.deref_mut() {
      return match write.shutdown().await {
        Ok(()) => 0,
        Err(e) => io_error_to_uv_code(&e),
      };
    }
    return UV_EBADF;
  }

  // Keep write_guard held to serialize with pending writes.
  if let Some(resource) = resource_snapshot {
    match resource.clone().shutdown().await {
      Ok(()) => 0,
      Err(_) => UV_UNKNOWN,
    }
  } else {
    UV_EBADF
  }
}

static ONCOMPLETE_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("oncomplete");

fn call_oncomplete(
  inner: &LibuvStreamInner,
  req: v8::Global<v8::Object>,
  status: i32,
) {
  inner.spawner.spawn(move |scope| {
    let req = v8::Local::new(scope, &req);
    let key = ONCOMPLETE_STR.v8_string(scope).unwrap();
    let Some(oncomplete) = req.get(scope, key.into()) else {
      return;
    };
    let Ok(oncomplete_fn) = oncomplete.try_cast::<v8::Function>() else {
      return;
    };
    let status_val: v8::Local<v8::Value> =
      v8::Integer::new(scope, status).into();
    let _ = oncomplete_fn.call(scope, req.into(), &[status_val]);
  });
}

// ---------------------------------------------------------------------------
// The cppgc object exposed to JS
// ---------------------------------------------------------------------------

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(HandleWrap)]
#[repr(C)]
pub struct LibuvStreamWrap {
  pub(crate) handle_wrap: HandleWrap,
  inner: Rc<LibuvStreamInner>,
}

// SAFETY: instances are prevented from preventing garbage collection
// by ensuring the stored Global is cleared on close.
unsafe impl GarbageCollected for LibuvStreamWrap {
  fn trace(&self, visitor: &mut v8::cppgc::Visitor) {
    self.handle_wrap.trace(visitor);
  }

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"LibuvStreamWrap"
  }
}

impl LibuvStreamWrap {
  pub(crate) fn create(
    this: v8::Global<v8::Object>,
    state: &mut OpState,
    provider: i32,
  ) -> LibuvStreamWrap {
    let spawner = state.borrow::<deno_core::V8TaskSpawner>().clone();
    let inner = Rc::new(LibuvStreamInner {
      read: Rc::new(AsyncRefCell::new(None)),
      write: Rc::new(AsyncRefCell::new(None)),
      resource: RefCell::new(None),
      resource_rid: Cell::new(None),
      #[cfg(unix)]
      raw_fd: Cell::new(None),
      cancel: RefCell::new(Rc::new(CancelHandle::new())),
      tls_upgrading: Cell::new(false),
      tls_upgrade_notify: tokio::sync::Notify::new(),
      reading: Cell::new(false),
      read_active: Cell::new(false),
      bytes_read: Cell::new(0),
      bytes_written: Cell::new(0),
      this,
      spawner,
      read_ref_tracker: super::RefTracker::new(
        state.external_ops_tracker.clone(),
      ),
      user_unrefed: Cell::new(false),
      peer_certificates: RefCell::new(None),
    });
    LibuvStreamWrap {
      handle_wrap: HandleWrap::create(AsyncWrap::create(state, provider), None),
      inner,
    }
  }

  /// Attach a Unix stream (pipe). Must be called before readStart().
  #[cfg(unix)]
  pub(crate) fn attach_unix_stream(&self, stream: tokio::net::UnixStream) {
    let (rd, wr) = stream.into_split();
    *self.inner.read.try_borrow_mut().unwrap() = Some(ReadHalf::Pipe(rd));
    *self.inner.write.try_borrow_mut().unwrap() = Some(WriteHalf::Pipe(wr));
  }

  /// Attach pre-split halves directly.
  pub(crate) fn attach_halves(&self, read: ReadHalf, write: WriteHalf) {
    // Capture raw fd from TCP write half for socket options.
    #[cfg(unix)]
    {
      use std::os::unix::io::AsRawFd;
      if let WriteHalf::Tcp(ref w) = write {
        self.inner.raw_fd.set(Some(w.as_ref().as_raw_fd()));
      }
    }
    *self.inner.read.try_borrow_mut().unwrap() = Some(read);
    *self.inner.write.try_borrow_mut().unwrap() = Some(write);
  }

  /// Get the raw fd of the underlying socket (unix only).
  #[cfg(unix)]
  pub(crate) fn raw_fd(&self) -> Option<RawFd> {
    self.inner.raw_fd.get()
  }

  /// Stop reading — callable from other modules (e.g. TCP._onClose).
  pub(crate) fn read_stop(&self) -> i32 {
    self.inner.reading.set(false);
    self.inner.cancel.borrow().cancel();
    *self.inner.cancel.borrow_mut() = Rc::new(CancelHandle::new());
    0
  }

  /// Close the underlying stream/resource. Called from `_onClose` so that
  /// the OS-level socket is actually shut down (sending FIN to the peer).
  pub(crate) fn close_stream(&self, state: &mut OpState) {
    // Stop reading and cancel any in-flight read.
    self.read_stop();
    // Eagerly unref the read loop so the event loop can exit even if the
    // read loop task hasn't had a chance to run its cleanup yet.
    self.unref_read();

    // Close the resource from the resource table (connect path).
    // This drops the TcpStreamResource, which drops both stream halves
    // and closes the socket.
    if let Some(rid) = self.inner.resource_rid.get() {
      if let Ok(resource) = state.resource_table.take_any(rid) {
        drop(resource);
      }
      self.inner.resource_rid.set(None);
    }

    // Clear the resource Rc reference.
    *self.inner.resource.borrow_mut() = None;

    // Drop the write half if not currently borrowed by a write task
    // (accept path with direct tokio halves).
    if let Some(mut guard) = self.inner.write.try_borrow_mut() {
      *guard = None;
    }

    // Drop the read half if not currently borrowed by the read loop.
    if let Some(mut guard) = self.inner.read.try_borrow_mut() {
      *guard = None;
    }
  }

  /// Take the native TCP halves out of this stream, stopping any
  /// in-progress read loop.  Returns `None` if the halves are not TCP
  /// or are currently borrowed.
  pub(crate) fn take_tcp_halves(
    &self,
  ) -> Option<(
    tokio::net::tcp::OwnedReadHalf,
    tokio::net::tcp::OwnedWriteHalf,
  )> {
    self.inner.reading.set(false);
    self.inner.cancel.borrow().cancel();
    *self.inner.cancel.borrow_mut() = Rc::new(CancelHandle::new());
    *self.inner.resource.borrow_mut() = None;
    self.inner.resource_rid.set(None);

    let mut rd_guard = self.inner.read.try_borrow_mut()?;
    let mut wr_guard = self.inner.write.try_borrow_mut()?;
    let rd = rd_guard.take();
    let wr = wr_guard.take();
    drop(rd_guard);
    drop(wr_guard);

    match (rd, wr) {
      (Some(ReadHalf::Tcp(rd)), Some(WriteHalf::Tcp(wr))) => Some((rd, wr)),
      _ => None,
    }
  }

  pub(crate) fn ref_read(&self) {
    self.inner.user_unrefed.set(false);
    ref_read_loop(&self.inner);
  }

  pub(crate) fn unref_read(&self) {
    self.inner.user_unrefed.set(true);
    unref_read_loop(&self.inner);
  }

}

#[op2(base, inherit = HandleWrap)]
impl LibuvStreamWrap {
  #[constructor]
  #[cppgc]
  fn new(
    #[this] this: v8::Global<v8::Object>,
    state: &mut OpState,
    #[smi] provider: i32,
  ) -> LibuvStreamWrap {
    LibuvStreamWrap::create(this, state, provider)
  }

  #[fast]
  #[smi]
  fn read_start(&self, state: &mut OpState) -> i32 {
    // Re-acquire the resource if readStop dropped it but the resource is
    // still in the table (e.g. after HTTP keepAlive return).
    if self.inner.resource.borrow().is_none() {
      if let Some(rid) = self.inner.resource_rid.get() {
        if let Ok(resource) = state.resource_table.get_any(rid) {
          *self.inner.resource.borrow_mut() = Some(resource);
        }
      }
    }
    start_read_loop(&self.inner);
    0
  }

  #[fast]
  #[smi]
  fn read_stop(&self) -> i32 {
    self.inner.reading.set(false);
    // Cancel any in-flight read, then replace the handle so the next
    // readStart() gets a fresh one.
    self.inner.cancel.borrow().cancel();
    *self.inner.cancel.borrow_mut() = Rc::new(CancelHandle::new());
    0
  }

  #[fast]
  #[rename("closeStream")]
  fn close_stream_js(&self, state: &mut OpState) {
    self.close_stream(state);
  }

  #[fast]
  fn detach_resource(&self) {
    *self.inner.resource.borrow_mut() = None;
    // Also clear the rid so readStart() can't re-acquire the resource
    // from the table after we've handed it off to hyper.
    self.inner.resource_rid.set(None);
  }

  #[reentrant]
  fn write_buffer(
    &self,
    #[scoped] req: v8::Global<v8::Object>,
    #[buffer] data: &[u8],
  ) -> i32 {
    let data = data.to_vec();
    let inner = self.inner.clone();
    // Clone the resource Rc synchronously (before spawning the async task)
    // so that a concurrent close_stream() triggered by another write's
    // oncomplete callback cannot drop it before the async task runs.
    let resource_snapshot = self.inner.resource.borrow().clone();

    deno_core::unsync::spawn(async move {
      let status =
        write_with_tls_retry(&inner, &data, resource_snapshot).await;
      call_oncomplete(&inner, req, status);
    });

    0
  }

  fn shutdown(&self, #[scoped] req: v8::Global<v8::Object>) -> i32 {
    let inner = self.inner.clone();
    let resource_snapshot = self.inner.resource.borrow().clone();

    deno_core::unsync::spawn(async move {
      let status =
        shutdown_with_tls_retry(&inner, resource_snapshot).await;
      call_oncomplete(&inner, req, status);
    });

    0
  }

  #[fast]
  fn attach_resource(&self, state: &mut OpState, #[smi] rid: u32) {
    if let Ok(resource) = state.resource_table.get_any(rid) {
      *self.inner.resource.borrow_mut() = Some(resource);
      self.inner.resource_rid.set(Some(rid));
    }
  }

  /// Return the peer certificate from a TLS-upgraded connection.
  /// Set during `upgradeTls` from the rustls handshake result.
  #[serde]
  #[rename("getPeerCertificate")]
  fn get_peer_certificate(
    &self,
    detailed: bool,
  ) -> Option<super::crypto::x509::CertificateObject> {
    let certs = self.inner.peer_certificates.borrow();
    let certs = certs.as_ref()?;
    if certs.is_empty() {
      return None;
    }
    let cert =
      super::crypto::x509::Certificate::from_der(certs[0].as_ref()).ok()?;
    cert.to_object(detailed).ok()
  }

  /// Take the stream halves, perform an HTTP/2 client handshake, and
  /// call back with (error, clientRid, connRid).  No intermediate
  /// storage – the TLS stream goes directly from this handle into h2.
  #[reentrant]
  #[rename("connectH2")]
  fn connect_h2(
    &self,
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
    #[scoped] callback: v8::Global<v8::Function>,
  ) -> i32 {
    // Stop reading so the read loop releases the borrow.
    self.read_stop();
    self.unref_read();

    let Some(mut rd_guard) = self.inner.read.try_borrow_mut() else {
      return UV_UNKNOWN;
    };
    let Some(mut wr_guard) = self.inner.write.try_borrow_mut() else {
      return UV_UNKNOWN;
    };
    let rd = rd_guard.take();
    let wr = wr_guard.take();
    drop(rd_guard);
    drop(wr_guard);

    use super::http2::H2Stream;

    let h2_stream = match (rd, wr) {
      (Some(ReadHalf::Tls(rd)), Some(WriteHalf::Tls(wr))) => {
        H2Stream::Tls(rd.unsplit(wr))
      }
      (Some(ReadHalf::Tcp(rd)), Some(WriteHalf::Tcp(wr))) => {
        match rd.reunite(wr) {
          Ok(s) => H2Stream::Tcp(s),
          Err(_) => return UV_UNKNOWN,
        }
      }
      _ => return UV_UNKNOWN,
    };

    let url = match url::Url::parse(&url) {
      Ok(u) => u,
      Err(_) => return UV_UNKNOWN,
    };

    let spawner = self.inner.spawner.clone();
    deno_core::unsync::spawn(async move {
      match h2::client::Builder::new().handshake(h2_stream).await {
        Ok((client, conn)) => {
          let (client_rid, conn_rid) = {
            let mut st = state.borrow_mut();
            let client_rid =
              st.resource_table
                .add(super::http2::Http2Client {
                  client: deno_core::AsyncRefCell::new(client),
                  url,
                });
            let conn_rid =
              st.resource_table
                .add(super::http2::Http2ClientConn {
                  conn: deno_core::AsyncRefCell::new(conn),
                  cancel_handle: deno_core::CancelHandle::new(),
                });
            (client_rid, conn_rid)
          };

          spawner.spawn(move |scope| {
            let cb = v8::Local::new(scope, &callback);
            let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
            let null_val: v8::Local<v8::Value> = v8::null(scope).into();
            let client_val: v8::Local<v8::Value> =
              v8::Integer::new_from_unsigned(scope, client_rid).into();
            let conn_val: v8::Local<v8::Value> =
              v8::Integer::new_from_unsigned(scope, conn_rid).into();
            cb.call(scope, recv, &[null_val, client_val, conn_val]);
          });
        }
        Err(e) => {
          spawner.spawn(move |scope| {
            let cb = v8::Local::new(scope, &callback);
            let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
            let msg = v8::String::new(scope, &e.to_string()).unwrap();
            let err_val: v8::Local<v8::Value> =
              v8::Exception::error(scope, msg);
            cb.call(scope, recv, &[err_val]);
          });
        }
      }
    });

    0
  }

  /// Take the stream halves, perform an HTTP/2 server handshake, and
  /// call back with (error, connRid).
  #[reentrant]
  #[rename("listenH2")]
  fn listen_h2(
    &self,
    state: Rc<RefCell<OpState>>,
    #[scoped] callback: v8::Global<v8::Function>,
  ) -> i32 {
    self.read_stop();
    self.unref_read();

    let Some(mut rd_guard) = self.inner.read.try_borrow_mut() else {
      return UV_UNKNOWN;
    };
    let Some(mut wr_guard) = self.inner.write.try_borrow_mut() else {
      return UV_UNKNOWN;
    };
    let rd = rd_guard.take();
    let wr = wr_guard.take();
    drop(rd_guard);
    drop(wr_guard);

    use super::http2::H2Stream;

    let h2_stream = match (rd, wr) {
      (Some(ReadHalf::Tls(rd)), Some(WriteHalf::Tls(wr))) => {
        H2Stream::Tls(rd.unsplit(wr))
      }
      (Some(ReadHalf::Tcp(rd)), Some(WriteHalf::Tcp(wr))) => {
        match rd.reunite(wr) {
          Ok(s) => H2Stream::Tcp(s),
          Err(_) => return UV_UNKNOWN,
        }
      }
      _ => return UV_UNKNOWN,
    };

    let spawner = self.inner.spawner.clone();
    deno_core::unsync::spawn(async move {
      match h2::server::Builder::new().handshake(h2_stream).await {
        Ok(conn) => {
          let conn_rid = {
            let mut st = state.borrow_mut();
            st.resource_table
              .add(super::http2::Http2ServerConnection {
                conn: deno_core::AsyncRefCell::new(conn),
              })
          };

          spawner.spawn(move |scope| {
            let cb = v8::Local::new(scope, &callback);
            let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
            let null_val: v8::Local<v8::Value> = v8::null(scope).into();
            let conn_val: v8::Local<v8::Value> =
              v8::Integer::new_from_unsigned(scope, conn_rid).into();
            cb.call(scope, recv, &[null_val, conn_val]);
          });
        }
        Err(e) => {
          spawner.spawn(move |scope| {
            let cb = v8::Local::new(scope, &callback);
            let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
            let msg = v8::String::new(scope, &e.to_string()).unwrap();
            let err_val: v8::Local<v8::Value> =
              v8::Exception::error(scope, msg);
            cb.call(scope, recv, &[err_val]);
          });
        }
      }
    });

    0
  }

  #[fast]
  fn set_handle(&self, state: &mut OpState, #[smi] rid: u32) {
    self.handle_wrap.set_handle(rid);
    state.uv_ref(rid);
  }

  #[fast]
  #[rename("ref")]
  fn stream_ref(&self, state: &mut OpState) {
    self.handle_wrap.ref_handle(state);
    self.ref_read();
  }

  #[fast]
  fn unref(&self, state: &mut OpState) {
    self.handle_wrap.unref_handle(state);
    self.unref_read();
  }

  /// Upgrade the underlying TCP connection to TLS (client-side).
  ///
  /// Takes the TCP read/write halves, reunites them into a TcpStream,
  /// wraps with rustls, performs the TLS handshake asynchronously, then
  /// stores the TLS read/write halves back. Calls the JS callback with
  /// (error, alpnProtocol) when done.
  #[reentrant]
  #[rename("upgradeTls")]
  fn upgrade_tls(
    &self,
    state: Rc<RefCell<OpState>>,
    #[string] hostname: String,
    #[serde] ca_certs: Option<Vec<String>>,
    #[serde] alpn_protocols: Option<Vec<String>>,
    reject_unauthorized: bool,
    #[string] cert: Option<String>,
    #[string] key: Option<String>,
    #[scoped] callback: v8::Global<v8::Function>,
  ) -> i32 {
    // 1. Take TCP halves synchronously.
    //    Mark the stream as upgrading so concurrent writes wait instead of
    //    returning UV_EBADF.
    self.inner.tls_upgrading.set(true);
    let Some(mut rd_guard) = self.inner.read.try_borrow_mut() else {
      self.inner.tls_upgrading.set(false);
      return UV_UNKNOWN;
    };
    let Some(mut wr_guard) = self.inner.write.try_borrow_mut() else {
      self.inner.tls_upgrading.set(false);
      return UV_UNKNOWN;
    };
    let rd = rd_guard.take();
    let wr = wr_guard.take();
    drop(rd_guard);
    drop(wr_guard);

    // 2. Reunite into TcpStream.
    let tcp_stream = match (rd, wr) {
      (Some(ReadHalf::Tcp(rd)), Some(WriteHalf::Tcp(wr))) => {
        match rd.reunite(wr) {
          Ok(s) => s,
          Err(_) => return UV_UNKNOWN,
        }
      }
      _ => return UV_UNKNOWN,
    };

    // 3. Build the TLS client config synchronously.
    let hostname = if hostname.is_empty() {
      "localhost".to_string()
    } else {
      hostname
    };
    let hostname_dns = match ServerName::try_from(hostname.clone()) {
      Ok(h) => h,
      Err(_) => return UV_UNKNOWN,
    };

    let state_ref = state.borrow();
    let unsafely_ignore = if reject_unauthorized {
      state_ref
        .try_borrow::<UnsafelyIgnoreCertificateErrors>()
        .and_then(|it| it.0.clone())
    } else {
      Some(Vec::new())
    };
    let root_cert_store =
      match state_ref.borrow::<DefaultTlsOptions>().root_cert_store() {
        Ok(s) => s,
        Err(_) => return UV_UNKNOWN,
      };
    drop(state_ref);

    let ca_cert_bytes: Vec<Vec<u8>> = ca_certs
      .unwrap_or_default()
      .into_iter()
      .map(|s| s.into_bytes())
      .collect();

    // Build TlsKeys from optional cert/key strings.
    let tls_keys = match (cert, key) {
      (Some(cert_pem), Some(key_pem)) => {
        let Ok(certs) = deno_tls::load_certs(
          &mut std::io::BufReader::new(cert_pem.as_bytes()),
        ) else {
          return UV_UNKNOWN;
        };
        let Ok(keys) = deno_tls::load_private_keys(key_pem.as_bytes()) else {
          return UV_UNKNOWN;
        };
        let Some(private_key) = keys.into_iter().next() else {
          return UV_UNKNOWN;
        };
        TlsKeys::Static(deno_tls::TlsKey(certs, private_key))
      }
      _ => TlsKeys::Null,
    };

    let mut tls_config = match create_client_config(TlsClientConfigOptions {
      root_cert_store,
      ca_certs: ca_cert_bytes,
      unsafely_ignore_certificate_errors: unsafely_ignore,
      unsafely_disable_hostname_verification: false,
      cert_chain_and_key: tls_keys,
      socket_use: SocketUse::GeneralSsl,
    }) {
      Ok(c) => c,
      Err(_) => return UV_UNKNOWN,
    };

    if let Some(alpn) = alpn_protocols {
      tls_config.alpn_protocols =
        alpn.into_iter().map(|s| s.into_bytes()).collect();
    }

    let tls_config = Arc::new(tls_config);

    // 4. Create the TLS stream wrapper (handshake hasn't happened yet).
    let tls_stream = match ClientConnection::new(tls_config, hostname_dns) {
      Ok(conn) => TlsStream::new_client_side(tcp_stream, conn, TLS_BUFFER_SIZE),
      Err(_) => return UV_UNKNOWN,
    };

    // 5. Spawn async task: handshake → split → store halves → callback.
    // Ref the event loop so it stays alive during the handshake.
    let ops_tracker = {
      let s = state.borrow();
      s.external_ops_tracker.clone()
    };
    ops_tracker.ref_op();

    let inner = self.inner.clone();
    deno_core::unsync::spawn(async move {
      let mut tls_stream = tls_stream;

      // Perform the TLS handshake.
      let hs_result = tls_stream.handshake().await;
      let alpn = match &hs_result {
        Ok(hs) => hs
          .alpn
          .as_ref()
          .and_then(|a| String::from_utf8(a.clone()).ok()),
        Err(_) => None,
      };
      // Store peer certificates for getPeerCertificate().
      if let Ok(hs) = &hs_result {
        *inner.peer_certificates.borrow_mut() =
          hs.peer_certificates.clone();
      }
      let hs_err = hs_result.err();

      // Split into halves and store back.
      let (rd, wr) = tls_stream.into_split();
      {
        let mut rd_guard = inner.read.borrow_mut().await;
        *rd_guard = Some(ReadHalf::Tls(rd));
      }
      {
        let mut wr_guard = inner.write.borrow_mut().await;
        *wr_guard = Some(WriteHalf::Tls(wr));
      }

      // Signal any writes that were waiting for the TLS upgrade to finish.
      inner.tls_upgrading.set(false);
      inner.tls_upgrade_notify.notify_waiters();

      // Callback to JS: callback(error, alpnProtocol).
      inner.spawner.spawn(move |scope| {
        let cb = v8::Local::new(scope, &callback);
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();

        let err_val: v8::Local<v8::Value> = if let Some(e) = hs_err {
          let msg = v8::String::new(scope, &e.to_string()).unwrap();
          v8::Exception::error(scope, msg).into()
        } else {
          v8::null(scope).into()
        };

        let alpn_val: v8::Local<v8::Value> = match alpn {
          Some(ref a) => v8::String::new(scope, a).unwrap().into(),
          None => v8::undefined(scope).into(),
        };

        cb.call(scope, recv, &[err_val, alpn_val]);

        // Unref the event loop now that the handshake is complete.
        ops_tracker.unref_op();
      });
    });

    0
  }

  #[getter]
  #[rename("bytesRead")]
  fn bytes_read(&self) -> f64 {
    self.inner.bytes_read.get() as f64
  }

  #[getter]
  #[rename("bytesWritten")]
  fn bytes_written(&self) -> f64 {
    self.inner.bytes_written.get() as f64
  }
}
