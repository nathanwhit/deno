// Copyright 2018-2026 the Deno authors. MIT license.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use deno_core::AsyncRefCell;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::CppgcBase;
use deno_core::CppgcInherits;
use deno_core::GarbageCollected;
use deno_core::OpState;
use deno_core::op2;
use deno_core::v8;

use super::RefTracker;
use super::connection_wrap::ConnectionWrap;
use super::stream_wrap::UV_UNKNOWN;
use super::stream_wrap::io_error_to_uv_code;

/// Socket type constants matching Node.js pipe_wrap.h
const SOCKET: i32 = 0;
const SERVER: i32 = 1;
const IPC: i32 = 2;

/// Node.js provider types for async tracking.
const PROVIDER_PIPEWRAP: i32 = 15;
const PROVIDER_PIPESERVERWRAP: i32 = 14;

/// Maximum length of a Unix socket path (sizeof(sockaddr_un.sun_path)).
/// libuv returns UV_EINVAL for paths at or exceeding this length.
#[cfg(unix)]
fn unix_socket_max_path() -> usize {
  // sockaddr_un.sun_path is 104 bytes on macOS, 108 on Linux.
  std::mem::size_of::<libc::sockaddr_un>()
    - std::mem::offset_of!(libc::sockaddr_un, sun_path)
}

/// Initial backoff delay of 5ms following a temporary accept failure.
const INITIAL_ACCEPT_BACKOFF_DELAY: u64 = 5;
/// Max backoff delay of 1s following a temporary accept failure.
const MAX_ACCEPT_BACKOFF_DELAY: u64 = 1000;

fn ceil_pow_of_2(n: u32) -> u32 {
  if n <= 1 {
    return 1;
  }
  1u32 << (32 - (n - 1).leading_zeros())
}

/// Shared state for the accept loop, stored in Rc so async tasks can
/// reference it after the cppgc object may have been collected.
struct PipeAcceptState {
  closed: Cell<bool>,
  cancel: Rc<CancelHandle>,
  server_this: RefCell<Option<v8::Global<v8::Object>>>,
  constructor: RefCell<Option<v8::Global<v8::Function>>>,
}

impl Drop for PipeAcceptState {
  fn drop(&mut self) {
    if let Some(g) = self.server_this.borrow_mut().take() {
      std::mem::forget(g);
    }
    if let Some(g) = self.constructor.borrow_mut().take() {
      std::mem::forget(g);
    }
  }
}

static ONCONNECTION_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("onconnection");

static AFTERCONNECT_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("afterConnect");

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(ConnectionWrap)]
#[repr(C)]
pub struct Pipe {
  connection_wrap: ConnectionWrap,

  ipc: Cell<bool>,
  address: RefCell<Option<String>>,

  // Server — store listener directly, NOT in resource table
  #[cfg(unix)]
  listener: Rc<AsyncRefCell<Option<tokio::net::UnixListener>>>,
  backlog: Cell<u32>,
  accept_state: Rc<PipeAcceptState>,
  ref_tracker: RefTracker,

  // Windows named pipe server rid (managed via ext/net ops from JS)
  server_pipe_rid: Cell<Option<u32>>,
  pending_instances: Cell<u32>,
}

// SAFETY: instances are prevented from preventing garbage collection
// by ensuring the stored Global is cleared on close.
unsafe impl GarbageCollected for Pipe {
  fn trace(&self, visitor: &mut v8::cppgc::Visitor) {
    self.connection_wrap.trace(visitor);
  }

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"Pipe"
  }
}

/// Call `onconnection(status, clientHandle)` on the server JS object.
fn call_onconnection(
  scope: &mut v8::PinScope<'_, '_>,
  server: v8::Local<v8::Object>,
  status: i32,
  client_handle: Option<v8::Local<v8::Object>>,
) {
  let key = ONCONNECTION_STR.v8_string(scope).unwrap();
  let Some(onconnection) = server.get(scope, key.into()) else {
    return;
  };
  let Ok(onconnection_fn) = onconnection.try_cast::<v8::Function>() else {
    return;
  };

  let status_val: v8::Local<v8::Value> = v8::Integer::new(scope, status).into();
  let client_val: v8::Local<v8::Value> = match client_handle {
    Some(h) => h.into(),
    None => v8::undefined(scope).into(),
  };

  let _ = onconnection_fn.call(scope, server.into(), &[status_val, client_val]);
}

/// Call `afterConnect(req, status)` on the handle JS object.
fn call_after_connect(
  scope: &mut v8::PinScope<'_, '_>,
  this: v8::Local<v8::Object>,
  req: &v8::Global<v8::Object>,
  status: i32,
) {
  let key = AFTERCONNECT_STR.v8_string(scope).unwrap();
  let Some(after_connect) = this.get(scope, key.into()) else {
    return;
  };
  let Ok(after_connect_fn) = after_connect.try_cast::<v8::Function>() else {
    return;
  };

  let req_local: v8::Local<v8::Value> = v8::Local::new(scope, req).into();
  let status_val: v8::Local<v8::Value> = v8::Integer::new(scope, status).into();

  let _ = after_connect_fn.call(scope, this.into(), &[req_local, status_val]);
}

/// Holds v8::Global values for an in-flight connect operation.
struct ConnectGlobals {
  this: RefCell<Option<v8::Global<v8::Object>>>,
  req: RefCell<Option<v8::Global<v8::Object>>>,
}

impl Drop for ConnectGlobals {
  fn drop(&mut self) {
    if let Some(g) = self.this.borrow_mut().take() {
      std::mem::forget(g);
    }
    if let Some(g) = self.req.borrow_mut().take() {
      std::mem::forget(g);
    }
  }
}

#[op2(inherit = ConnectionWrap)]
impl Pipe {
  #[constructor]
  #[cppgc]
  fn new(
    #[this] this: v8::Global<v8::Object>,
    state: &mut OpState,
    #[smi] r#type: i32,
  ) -> Pipe {
    let (provider, ipc) = match r#type {
      SOCKET => (PROVIDER_PIPEWRAP, false),
      SERVER => (PROVIDER_PIPESERVERWRAP, false),
      IPC => (PROVIDER_PIPEWRAP, true),
      _ => (PROVIDER_PIPEWRAP, false),
    };

    Pipe {
      connection_wrap: ConnectionWrap::create(this, state, provider),
      ipc: Cell::new(ipc),
      address: RefCell::new(None),
      #[cfg(unix)]
      listener: Rc::new(AsyncRefCell::new(None)),
      backlog: Cell::new(0),
      accept_state: Rc::new(PipeAcceptState {
        closed: Cell::new(false),
        cancel: Rc::new(CancelHandle::new()),
        server_this: RefCell::new(None),
        constructor: RefCell::new(None),
      }),
      ref_tracker: RefTracker::new(state.external_ops_tracker.clone()),
      server_pipe_rid: Cell::new(None),
      pending_instances: Cell::new(4),
    }
  }

  #[getter]
  fn ipc(&self) -> bool {
    self.ipc.get()
  }

  #[fast]
  #[smi]
  fn bind(&self, #[string] name: String) -> i32 {
    *self.address.borrow_mut() = Some(name);
    0
  }

  /// Open a pipe from an existing file descriptor.
  /// Returns 0 on success, or a negative UV error code.
  /// Returns `-ENOTSOCK` if the fd is not a socket (JS caller should
  /// try a file-based fallback).
  #[fast]
  #[smi]
  fn open(&self, #[smi] fd: i32) -> i32 {
    #[cfg(unix)]
    {
      use std::os::unix::io::FromRawFd;

      // Check if the fd is a socket BEFORE consuming it with from_raw_fd,
      // because tokio::net::UnixStream::from_std will close the fd on
      // failure and the JS caller needs the fd intact for a file fallback.
      let is_socket = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut stat) == 0
          && (stat.st_mode & libc::S_IFMT == libc::S_IFSOCK)
      };

      if !is_socket {
        return -(libc::ENOTSOCK as i32);
      }

      let std_stream =
        unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
      if let Err(e) = std_stream.set_nonblocking(true) {
        // Put the fd back so we don't close it.
        let _ = std::os::unix::io::IntoRawFd::into_raw_fd(std_stream);
        return io_error_to_uv_code(&e);
      }
      match tokio::net::UnixStream::from_std(std_stream) {
        Ok(stream) => {
          self
            .connection_wrap
            .stream_wrap
            .attach_unix_stream(stream);
          0
        }
        Err(e) => io_error_to_uv_code(&e),
      }
    }

    #[cfg(not(unix))]
    {
      let _ = fd;
      UV_UNKNOWN
    }
  }

  /// Connect to a Unix domain socket.
  #[cfg(unix)]
  #[reentrant]
  fn connect(
    &self,
    #[this] this: v8::Global<v8::Object>,
    op_state: Rc<RefCell<OpState>>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] address: String,
  ) -> i32 {
    *self.address.borrow_mut() = Some(address.clone());

    // Validate path length, matching libuv (returns EINVAL for too-long paths).
    if address.as_bytes().len() >= unix_socket_max_path() {
      return -22; // UV_EINVAL
    }

    self.ref_tracker.ref_();

    let spawner = op_state
      .borrow()
      .borrow::<deno_core::V8TaskSpawner>()
      .clone();

    let globals = Rc::new(ConnectGlobals {
      this: RefCell::new(Some(this)),
      req: RefCell::new(Some(req)),
    });

    // The ref is removed inside the V8 callback, after afterConnect has
    // run. This keeps the event loop alive during connect but doesn't
    // keep it alive permanently afterwards.
    let ref_tracker = self.ref_tracker.clone();
    deno_core::unsync::spawn(async move {
      match tokio::net::UnixStream::connect(&address).await {
        Ok(stream) => {
          let globals2 = globals.clone();
          let ref_tracker2 = ref_tracker.clone();
          spawner.spawn(move |scope| {
            let this_g = globals2.this.borrow_mut().take();
            let req_g = globals2.req.borrow_mut().take();
            let (Some(this_g), Some(req_g)) = (this_g, req_g) else {
              ref_tracker2.unref();
              return;
            };
            let this_local = v8::Local::new(scope, &this_g);

            let pipe =
              deno_core::cppgc::try_unwrap_cppgc_object::<Pipe>(
                scope,
                this_local.into(),
              )
              .unwrap();

            pipe
              .connection_wrap
              .stream_wrap
              .attach_unix_stream(stream);

            call_after_connect(scope, this_local, &req_g, 0);
            ref_tracker2.unref();
          });
        }
        Err(e) => {
          let code = io_error_to_uv_code(&e);
          let globals2 = globals.clone();
          let ref_tracker2 = ref_tracker.clone();
          spawner.spawn(move |scope| {
            let this_g = globals2.this.borrow_mut().take();
            let req_g = globals2.req.borrow_mut().take();
            let (Some(this_g), Some(req_g)) = (this_g, req_g) else {
              ref_tracker2.unref();
              return;
            };
            let this_local = v8::Local::new(scope, &this_g);
            call_after_connect(scope, this_local, &req_g, code);
            ref_tracker2.unref();
          });
        }
      }
    });

    0
  }

  /// Listen for new connections on a Unix domain socket.
  #[cfg(unix)]
  #[nofast]
  #[reentrant]
  fn listen(
    &self,
    #[this] this: v8::Global<v8::Object>,
    op_state: Rc<RefCell<OpState>>,
    #[smi] backlog: i32,
    scope: &mut v8::PinScope<'_, '_>,
  ) -> i32 {
    let this_local = v8::Local::new(scope, &this);
    let key = v8::String::new(scope, "constructor").unwrap();
    let Some(ctor_val) = this_local.get(scope, key.into()) else {
      return UV_UNKNOWN;
    };
    let Ok(constructor_local) = ctor_val.try_cast::<v8::Function>() else {
      return UV_UNKNOWN;
    };
    let constructor = v8::Global::new(scope, constructor_local);

    let backlog = ceil_pow_of_2((backlog + 1) as u32);
    self.backlog.set(backlog);

    let address = self.address.borrow().clone().unwrap_or_default();

    // Validate path length, matching libuv's check (returns EINVAL for
    // paths that exceed sockaddr_un.sun_path).
    if address.as_bytes().len() >= unix_socket_max_path() {
      return -22; // UV_EINVAL
    }

    // Bind + listen.
    let listener = match tokio::net::UnixListener::bind(&address) {
      Ok(l) => l,
      Err(e) => return io_error_to_uv_code(&e),
    };

    // Store the listener directly on the struct.
    *self.listener.try_borrow_mut().unwrap() = Some(listener);

    // Update the stored address to the actual bound path.
    if let Some(ref mut addr) = *self.address.borrow_mut() {
      *addr = address;
    }

    // Keep the event loop alive.
    self.ref_tracker.ref_();

    *self.accept_state.constructor.borrow_mut() = Some(constructor);
    *self.accept_state.server_this.borrow_mut() = Some(this);

    // Spawn the accept loop.
    let accept_state = self.accept_state.clone();
    let listener_rc = self.listener.clone();
    let spawner = op_state
      .borrow()
      .borrow::<deno_core::V8TaskSpawner>()
      .clone();

    deno_core::unsync::spawn(async move {
      let mut accept_backoff_delay: Option<u64> = None;

      loop {
        if accept_state.closed.get() {
          break;
        }

        let result =
          accept_from_listener(&listener_rc, &accept_state.cancel).await;

        match result {
          Ok(stream) => {
            accept_backoff_delay = None;

            let accept_state2 = accept_state.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            spawner.spawn(move |scope| {
              let constructor_guard = accept_state2.constructor.borrow();
              let Some(constructor_global) = constructor_guard.as_ref() else {
                let _ = tx.send(());
                return;
              };
              let ctor = v8::Local::new(scope, constructor_global);
              drop(constructor_guard);

              let arg: v8::Local<v8::Value> =
                v8::Integer::new(scope, SOCKET).into();
              let Some(handle_obj) = ctor.new_instance(scope, &[arg]) else {
                let _ = tx.send(());
                return;
              };

              let pipe =
                deno_core::cppgc::try_unwrap_cppgc_object::<Pipe>(
                  scope,
                  handle_obj.into(),
                )
                .unwrap();

              pipe
                .connection_wrap
                .stream_wrap
                .attach_unix_stream(stream);

              let server_guard = accept_state2.server_this.borrow();
              let Some(server_global) = server_guard.as_ref() else {
                let _ = tx.send(());
                return;
              };
              let server = v8::Local::new(scope, server_global);
              drop(server_guard);

              call_onconnection(scope, server, 0, Some(handle_obj));
              let _ = tx.send(());
            });
            let _ = rx.await;
          }
          Err(_) => {
            if accept_state.closed.get() {
              break;
            }

            let accept_state2 = accept_state.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            spawner.spawn(move |scope| {
              let server_guard = accept_state2.server_this.borrow();
              let Some(server_global) = server_guard.as_ref() else {
                let _ = tx.send(());
                return;
              };
              let server = v8::Local::new(scope, server_global);
              drop(server_guard);

              call_onconnection(scope, server, UV_UNKNOWN, None);
              let _ = tx.send(());
            });
            let _ = rx.await;

            let delay_ms =
              accept_backoff_delay.unwrap_or(INITIAL_ACCEPT_BACKOFF_DELAY);
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
              .await;
            accept_backoff_delay =
              Some((delay_ms * 2).min(MAX_ACCEPT_BACKOFF_DELAY));
          }
        }
      }
    });

    0
  }

  /// Alter pipe permissions (Unix only).
  #[fast]
  #[smi]
  fn fchmod(&self, #[smi] mode: i32) -> i32 {
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;

      const UV_READABLE: i32 = 1;
      const UV_WRITABLE: i32 = 2;

      if mode != UV_READABLE
        && mode != UV_WRITABLE
        && mode != (UV_READABLE | UV_WRITABLE)
      {
        return -(libc::EINVAL as i32);
      }

      let mut desired_mode: u32 = 0;
      if mode & UV_READABLE != 0 {
        desired_mode |= 0o444; // S_IRUSR | S_IRGRP | S_IROTH
      }
      if mode & UV_WRITABLE != 0 {
        desired_mode |= 0o222; // S_IWUSR | S_IWGRP | S_IWOTH
      }

      let address = self.address.borrow();
      let Some(path) = address.as_ref() else {
        return UV_UNKNOWN;
      };
      match std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(desired_mode),
      ) {
        Ok(()) => 0,
        Err(e) => io_error_to_uv_code(&e),
      }
    }

    #[cfg(not(unix))]
    {
      let _ = mode;
      UV_UNKNOWN
    }
  }

  #[fast]
  #[rename("setPendingInstances")]
  fn set_pending_instances(&self, #[smi] instances: i32) {
    self.pending_instances.set(instances as u32);
  }

  #[fast]
  #[rename("_onClose")]
  fn on_close(&self, state: &mut OpState) -> i32 {
    self.accept_state.closed.set(true);
    self.accept_state.cancel.cancel();

    *self.address.borrow_mut() = None;
    self.backlog.set(0);

    // Drop the Unix listener directly.
    #[cfg(unix)]
    {
      if let Some(mut guard) = self.listener.try_borrow_mut() {
        *guard = None;
      }
    }

    // Close Windows pipe server resource.
    if let Some(rid) = self.server_pipe_rid.get() {
      if let Ok(resource) = state.resource_table.take_any(rid) {
        drop(resource);
      }
      self.server_pipe_rid.set(None);
    }

    // Clear v8::Global values.
    self.accept_state.constructor.borrow_mut().take();
    self.accept_state.server_this.borrow_mut().take();

    // Close the underlying stream.
    self.connection_wrap.stream_wrap.close_stream(state);

    self.ref_tracker.unref();
    0
  }

  #[fast]
  #[rename("ref")]
  fn pipe_ref(&self, state: &mut OpState) {
    self
      .connection_wrap
      .stream_wrap
      .handle_wrap
      .ref_handle(state);
    self.connection_wrap.stream_wrap.ref_read();
    self.ref_tracker.ref_();
  }

  #[fast]
  fn unref(&self, state: &mut OpState) {
    self
      .connection_wrap
      .stream_wrap
      .handle_wrap
      .unref_handle(state);
    self.connection_wrap.stream_wrap.unref_read();
    self.ref_tracker.unref();
  }
}

/// Accept a connection from a Unix listener stored directly on the struct.
#[cfg(unix)]
async fn accept_from_listener(
  listener_rc: &Rc<AsyncRefCell<Option<tokio::net::UnixListener>>>,
  cancel: &Rc<CancelHandle>,
) -> Result<tokio::net::UnixStream, std::io::Error> {
  let accept_fut = async {
    let mut guard = listener_rc.borrow_mut().await;
    let opt: &mut Option<tokio::net::UnixListener> = &mut *guard;
    match opt {
      Some(listener) => {
        let (stream, _addr) = listener.accept().await?;
        Ok(stream)
      }
      None => Err(std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "listener closed",
      )),
    }
  };

  let result = accept_fut.or_cancel(cancel.clone()).await;
  result.map_err(|_| {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "listener closed")
  })?
}
