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
use super::stream_wrap::ReadHalf;
use super::stream_wrap::UV_UNKNOWN;
use super::stream_wrap::WriteHalf;
use super::stream_wrap::io_error_to_uv_code;

/// Socket type constants matching Node.js tcp_wrap.h
const SOCKET: i32 = 0;
const SERVER: i32 = 1;

/// Node.js provider types for async tracking.
const PROVIDER_TCPWRAP: i32 = 19; // providerType.TCPWRAP
const PROVIDER_TCPSERVERWRAP: i32 = 18; // providerType.TCPSERVERWRAP

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

fn get_ip_family(address: &str) -> &'static str {
  if address.contains(':') {
    "IPv6"
  } else {
    "IPv4"
  }
}

/// Shared state for the accept loop, stored in Rc so async tasks can
/// reference it after the cppgc object may have been collected.
///
/// The v8::Global values are stored here (not in the async task) so
/// they can be cleared in `_onClose` before the isolate is disposed.
struct AcceptState {
  closed: Cell<bool>,
  cancel: Rc<CancelHandle>,
  server_this: RefCell<Option<v8::Global<v8::Object>>>,
  constructor: RefCell<Option<v8::Global<v8::Function>>>,
}

impl Drop for AcceptState {
  fn drop(&mut self) {
    // If globals haven't been cleared by _onClose, leak them to avoid
    // panicking when dropping after the V8 isolate is disposed.
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

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(ConnectionWrap)]
#[repr(C)]
pub struct TCP {
  pub(crate) connection_wrap: ConnectionWrap,

  address: RefCell<Option<String>>,
  port: Cell<Option<u16>>,

  remote_address: RefCell<Option<String>>,
  remote_port: Cell<Option<u16>>,
  remote_family: RefCell<Option<String>>,

  /// A pre-bound socket created by bind()/bind6(), used by listen() or connect().
  bound_socket: RefCell<Option<socket2::Socket>>,

  listener: Rc<AsyncRefCell<Option<tokio::net::TcpListener>>>,
  backlog: Cell<u32>,
  accept_state: Rc<AcceptState>,
  pub(crate) ref_tracker: RefTracker,
}

// SAFETY: instances are prevented from preventing garbage collection
// by ensuring the stored Global is cleared on close.
unsafe impl GarbageCollected for TCP {
  fn trace(&self, visitor: &mut v8::cppgc::Visitor) {
    self.connection_wrap.trace(visitor);
  }

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"TCP"
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
  let key = v8::String::new(scope, "afterConnect").unwrap();
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


#[op2(inherit = ConnectionWrap)]
impl TCP {
  #[constructor]
  #[cppgc]
  fn new(
    #[this] this: v8::Global<v8::Object>,
    state: &mut OpState,
    #[smi] r#type: i32,
  ) -> TCP {
    let provider = match r#type {
      SOCKET => PROVIDER_TCPWRAP,
      SERVER => PROVIDER_TCPSERVERWRAP,
      _ => PROVIDER_TCPWRAP,
    };

    TCP {
      connection_wrap: ConnectionWrap::create(this, state, provider),
      address: RefCell::new(None),
      port: Cell::new(None),
      remote_address: RefCell::new(None),
      remote_port: Cell::new(None),
      remote_family: RefCell::new(None),
      bound_socket: RefCell::new(None),
      listener: Rc::new(AsyncRefCell::new(None)),
      backlog: Cell::new(0),
      accept_state: Rc::new(AcceptState {
        closed: Cell::new(false),
        cancel: Rc::new(CancelHandle::new()),
        server_this: RefCell::new(None),
        constructor: RefCell::new(None),
      }),
      ref_tracker: RefTracker::new(state.external_ops_tracker.clone()),
    }
  }

  #[fast]
  #[smi]
  fn bind(&self, #[string] address: String, #[smi] port: u16) -> i32 {
    do_bind(
      &self.address,
      &self.port,
      &self.bound_socket,
      &address,
      port,
      socket2::Domain::IPV4,
    )
  }

  #[fast]
  #[smi]
  fn bind6(
    &self,
    #[string] address: String,
    #[smi] port: u16,
    #[smi] _flags: i32,
  ) -> i32 {
    do_bind(
      &self.address,
      &self.port,
      &self.bound_socket,
      &address,
      port,
      socket2::Domain::IPV6,
    )
  }

  #[nofast]
  #[reentrant]
  fn listen(
    &self,
    #[this] this: v8::Global<v8::Object>,
    op_state: Rc<RefCell<OpState>>,
    #[smi] backlog: i32,
    scope: &mut v8::PinScope<'_, '_>,
  ) -> i32 {
    // Get this.constructor for the accept loop to create new instances.
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

    // Take the pre-bound socket (from bind()/bind6()), or fail if not bound.
    let Some(socket) = self.bound_socket.borrow_mut().take() else {
      return UV_UNKNOWN;
    };

    // Call listen on the bound socket, then convert to a tokio TcpListener.
    if let Err(e) = socket.listen(backlog as i32) {
      return io_error_to_uv_code(&e);
    }
    if let Err(e) = socket.set_nonblocking(true) {
      return io_error_to_uv_code(&e);
    }
    let std_listener: std::net::TcpListener = socket.into();
    let listener = match tokio::net::TcpListener::from_std(std_listener) {
      Ok(l) => l,
      Err(e) => return io_error_to_uv_code(&e),
    };
    let local_addr = match listener.local_addr() {
      Ok(a) => a,
      Err(e) => return io_error_to_uv_code(&e),
    };

    // Update address/port with the actual bound address.
    *self.address.borrow_mut() = Some(local_addr.ip().to_string());
    self.port.set(Some(local_addr.port()));

    // Store the listener directly on the struct (no resource table).
    *self.listener.try_borrow_mut().unwrap() = Some(listener);

    // Keep the event loop alive while the server is listening.
    self.ref_tracker.ref_();

    // Store the constructor and server handle in AcceptState so they
    // can be cleaned up in _onClose before the isolate is disposed.
    // The async accept loop accesses them via Rc<AcceptState> and
    // never holds v8::Global values directly in its state machine.
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
          Ok((stream, local_addr, remote_addr)) => {
            accept_backoff_delay = None;
            let (rd, wr) = stream.into_split();

            let accept_state2 = accept_state.clone();

            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            spawner.spawn(move |scope| {
              let constructor_guard = accept_state2.constructor.borrow();
              let Some(constructor_global) = constructor_guard.as_ref() else {
                let _ = tx.send(());
                return;
              };
              let constructor = v8::Local::new(scope, constructor_global);
              drop(constructor_guard);

              let arg: v8::Local<v8::Value> =
                v8::Integer::new(scope, SOCKET).into();
              let Some(handle_obj) = constructor.new_instance(scope, &[arg])
              else {
                let _ = tx.send(());
                return;
              };

              let tcp = deno_core::cppgc::try_unwrap_cppgc_object::<TCP>(
                scope,
                handle_obj.into(),
              )
              .unwrap();

              tcp
                .connection_wrap
                .stream_wrap
                .attach_halves(ReadHalf::Tcp(rd), WriteHalf::Tcp(wr));

              *tcp.address.borrow_mut() = Some(local_addr.ip().to_string());
              tcp.port.set(Some(local_addr.port()));
              *tcp.remote_address.borrow_mut() =
                Some(remote_addr.ip().to_string());
              tcp.remote_port.set(Some(remote_addr.port()));
              *tcp.remote_family.borrow_mut() =
                Some(get_ip_family(&remote_addr.ip().to_string()).to_string());

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

  #[reentrant]
  fn connect(
    &self,
    #[this] this: v8::Global<v8::Object>,
    op_state: Rc<RefCell<OpState>>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] address: String,
    #[smi] port: u16,
  ) -> i32 {
    *self.remote_address.borrow_mut() = Some(address.clone());
    self.remote_port.set(Some(port));
    *self.remote_family.borrow_mut() =
      Some(get_ip_family(&address).to_string());

    self.ref_tracker.ref_();

    // Take the pre-bound socket if bind() was called.
    let bound_socket = self.bound_socket.borrow_mut().take();

    let spawner = op_state
      .borrow()
      .borrow::<deno_core::V8TaskSpawner>()
      .clone();

    do_connect(
      spawner,
      op_state,
      this,
      req,
      address,
      port,
      false,
      self.ref_tracker.clone(),
      bound_socket,
    );
    0
  }

  #[reentrant]
  fn connect6(
    &self,
    #[this] this: v8::Global<v8::Object>,
    op_state: Rc<RefCell<OpState>>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] address: String,
    #[smi] port: u16,
  ) -> i32 {
    *self.remote_address.borrow_mut() = Some(address.clone());
    self.remote_port.set(Some(port));
    *self.remote_family.borrow_mut() =
      Some(get_ip_family(&address).to_string());

    self.ref_tracker.ref_();

    let bound_socket = self.bound_socket.borrow_mut().take();

    let spawner = op_state
      .borrow()
      .borrow::<deno_core::V8TaskSpawner>()
      .clone();

    do_connect(
      spawner,
      op_state,
      this,
      req,
      address,
      port,
      true,
      self.ref_tracker.clone(),
      bound_socket,
    );
    0
  }

  #[reentrant]
  #[smi]
  fn getsockname(
    &self,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] out: v8::Global<v8::Object>,
  ) -> i32 {
    let out = v8::Local::new(scope, &out);
    let address = self.address.borrow();
    let Some(addr) = address.as_ref() else {
      return UV_UNKNOWN;
    };
    let port = self.port.get();
    let Some(port) = port else {
      return UV_UNKNOWN;
    };

    let addr_key = v8::String::new(scope, "address").unwrap();
    let addr_val = v8::String::new(scope, addr).unwrap();
    out.set(scope, addr_key.into(), addr_val.into());

    let port_key = v8::String::new(scope, "port").unwrap();
    let port_val = v8::Integer::new(scope, port as i32);
    out.set(scope, port_key.into(), port_val.into());

    let family_key = v8::String::new(scope, "family").unwrap();
    let family_val = v8::String::new(scope, get_ip_family(addr)).unwrap();
    out.set(scope, family_key.into(), family_val.into());

    0
  }

  #[reentrant]
  #[smi]
  fn getpeername(
    &self,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] out: v8::Global<v8::Object>,
  ) -> i32 {
    let out = v8::Local::new(scope, &out);
    let address = self.remote_address.borrow();
    let Some(addr) = address.as_ref() else {
      return UV_UNKNOWN;
    };
    let port = self.remote_port.get();
    let Some(port) = port else {
      return UV_UNKNOWN;
    };

    let addr_key = v8::String::new(scope, "address").unwrap();
    let addr_val = v8::String::new(scope, addr).unwrap();
    out.set(scope, addr_key.into(), addr_val.into());

    let port_key = v8::String::new(scope, "port").unwrap();
    let port_val = v8::Integer::new(scope, port as i32);
    out.set(scope, port_key.into(), port_val.into());

    let family_key = v8::String::new(scope, "family").unwrap();
    let family = self.remote_family.borrow();
    let family_str = family.as_deref().unwrap_or("IPv4");
    let family_val = v8::String::new(scope, family_str).unwrap();
    out.set(scope, family_key.into(), family_val.into());

    0
  }

  #[fast]
  #[smi]
  fn set_no_delay(&self, enable: bool) -> i32 {
    #[cfg(unix)]
    {
      use std::os::unix::io::BorrowedFd;
      if let Some(fd) = self.connection_wrap.stream_wrap.raw_fd() {
        // SAFETY: The fd is valid as long as the TCP stream is alive.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let sock = socket2::SockRef::from(&borrowed);
        if sock.set_nodelay(enable).is_err() {
          return UV_UNKNOWN;
        }
      }
    }
    0
  }

  #[fast]
  #[smi]
  fn set_keep_alive(&self, enable: bool, delay: i32) -> i32 {
    #[cfg(unix)]
    {
      use std::os::unix::io::BorrowedFd;
      if let Some(fd) = self.connection_wrap.stream_wrap.raw_fd() {
        // SAFETY: The fd is valid as long as the TCP stream is alive.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let sock = socket2::SockRef::from(&borrowed);
        if sock.set_keepalive(enable).is_err() {
          return UV_UNKNOWN;
        }
        if enable && delay > 0 {
          let keepalive = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(delay as u64));
          let _ = sock.set_tcp_keepalive(&keepalive);
        }
      }
    }
    0
  }

  #[fast]
  #[rename("_onClose")]
  fn on_close(&self, state: &mut OpState) -> i32 {
    self.accept_state.closed.set(true);
    // Cancel any in-flight accept.
    self.accept_state.cancel.cancel();

    *self.address.borrow_mut() = None;
    self.port.set(None);

    *self.remote_address.borrow_mut() = None;
    *self.remote_family.borrow_mut() = None;
    self.remote_port.set(None);

    self.backlog.set(0);

    // Drop the listener directly if the borrow is available.
    if let Some(mut guard) = self.listener.try_borrow_mut() {
      *guard = None;
    }

    // Clear v8::Global values from AcceptState while the isolate is
    // still alive. This prevents the accept loop's async task from
    // panicking when dropped after isolate disposal.
    self.accept_state.constructor.borrow_mut().take();
    self.accept_state.server_this.borrow_mut().take();

    // Close the underlying stream/resource so the OS socket is actually
    // shut down (sends FIN to the peer).
    self.connection_wrap.stream_wrap.close_stream(state);

    // Allow the event loop to exit (only if we called ref_).
    self.ref_tracker.unref();
    0
  }

  #[fast]
  #[rename("ref")]
  fn tcp_ref(&self, state: &mut OpState) {
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

/// Holds v8::Global values for an in-flight connect operation.
/// The Drop impl leaks any remaining globals to avoid panicking
/// when the async task is dropped after the V8 isolate is disposed.
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

/// Actually bind a socket to the given address/port, storing the result.
/// Returns 0 on success, or a negative UV error code on failure.
fn do_bind(
  address_out: &RefCell<Option<String>>,
  port_out: &Cell<Option<u16>>,
  socket_out: &RefCell<Option<socket2::Socket>>,
  address: &str,
  port: u16,
  domain: socket2::Domain,
) -> i32 {
  let ip: std::net::IpAddr = match address.parse() {
    Ok(ip) => ip,
    Err(_) => return UV_UNKNOWN,
  };
  let addr = std::net::SocketAddr::new(ip, port);

  let socket = match socket2::Socket::new(
    domain,
    socket2::Type::STREAM,
    Some(socket2::Protocol::TCP),
  ) {
    Ok(s) => s,
    Err(e) => return io_error_to_uv_code(&e),
  };

  // Match libuv: set SO_REUSEADDR.
  let _ = socket.set_reuse_address(true);

  if let Err(e) = socket.bind(&addr.into()) {
    return io_error_to_uv_code(&e);
  }

  // Read back the actual bound address (port may differ if 0 was requested).
  if let Ok(local) = socket.local_addr() {
    if let Some(local) = local.as_socket() {
      *address_out.borrow_mut() = Some(local.ip().to_string());
      port_out.set(Some(local.port()));
    }
  } else {
    *address_out.borrow_mut() = Some(address.to_string());
    port_out.set(Some(port));
  }

  *socket_out.borrow_mut() = Some(socket);
  0
}

/// Shared connect implementation for connect/connect6.
///
/// The caller must have called `ref_tracker.ref_()` before calling this.
/// The ref is removed inside the V8 callback, after `afterConnect` has
/// run.  This ensures the event loop stays alive during the connect but
/// doesn't keep it alive permanently afterwards (reading/writing have
/// their own ref tracking).
fn do_connect(
  spawner: deno_core::V8TaskSpawner,
  _op_state: Rc<RefCell<OpState>>,
  this: v8::Global<v8::Object>,
  req: v8::Global<v8::Object>,
  address: String,
  port: u16,
  is_ipv6: bool,
  ref_tracker: RefTracker,
  bound_socket: Option<socket2::Socket>,
) {
  let globals = Rc::new(ConnectGlobals {
    this: RefCell::new(Some(this)),
    req: RefCell::new(Some(req)),
  });

  deno_core::unsync::spawn(async move {
    let addr = if is_ipv6 && address.contains(':') {
      format!("[{address}]:{port}")
    } else {
      format!("{address}:{port}")
    };
    let resolved = match tokio::net::lookup_host(&addr).await {
      Ok(mut addrs) => addrs.next(),
      Err(_) => None,
    };

    let Some(socket_addr) = resolved else {
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
        call_after_connect(scope, this_local, &req_g, UV_UNKNOWN);
        ref_tracker2.unref();
      });
      return;
    };

    let connect_result = if let Some(socket) = bound_socket {
      // We have a pre-bound socket2::Socket from bind().
      // Set it to non-blocking before converting to tokio.
      if let Err(e) = socket.set_nonblocking(true) {
        Err(e)
      } else {
        let std_stream: std::net::TcpStream = socket.into();
        match tokio::net::TcpSocket::from_std_stream(std_stream) {
          sock => sock.connect(socket_addr).await,
        }
      }
    } else {
      tokio::net::TcpStream::connect(socket_addr).await
    };

    match connect_result {
      Ok(tcp_stream) => {
        let local = tcp_stream.local_addr().ok();
        let remote = tcp_stream.peer_addr().ok();

        // Split into native tokio halves for direct read/write.
        let (rd, wr) = tcp_stream.into_split();

        let globals2 = globals.clone();
        spawner.spawn(move |scope| {
          let this_g = globals2.this.borrow_mut().take();
          let req_g = globals2.req.borrow_mut().take();
          let (Some(this_g), Some(req_g)) = (this_g, req_g) else {
            return;
          };
          let this_local = v8::Local::new(scope, &this_g);

          let tcp = deno_core::cppgc::try_unwrap_cppgc_object::<TCP>(
            scope,
            this_local.into(),
          )
          .unwrap();

          let (local_addr_str, local_port_val) = if let Some(addr) = &local {
            (addr.ip().to_string(), addr.port())
          } else {
            (String::new(), 0)
          };
          let (remote_addr_str, remote_port_val) = if let Some(addr) = &remote {
            (addr.ip().to_string(), addr.port())
          } else {
            (String::new(), 0)
          };

          *tcp.address.borrow_mut() = Some(local_addr_str.clone());
          tcp.port.set(Some(local_port_val));
          *tcp.remote_address.borrow_mut() = Some(remote_addr_str.clone());
          tcp.remote_port.set(Some(remote_port_val));
          *tcp.remote_family.borrow_mut() =
            Some(get_ip_family(&remote_addr_str).to_string());

          // Attach native tokio halves for read/write.
          tcp.connection_wrap.stream_wrap.attach_halves(
            ReadHalf::Tcp(rd),
            WriteHalf::Tcp(wr),
          );

          let req_local = v8::Local::new(scope, &req_g);
          let la_key = v8::String::new(scope, "localAddress").unwrap();
          let la_val = v8::String::new(scope, &local_addr_str).unwrap();
          req_local.set(scope, la_key.into(), la_val.into());
          let lp_key = v8::String::new(scope, "localPort").unwrap();
          let lp_val = v8::Integer::new(scope, local_port_val as i32);
          req_local.set(scope, lp_key.into(), lp_val.into());

          call_after_connect(scope, this_local, &req_g, 0);
          ref_tracker.unref();
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
}

/// Accept a connection from a listener stored directly on the struct.
async fn accept_from_listener(
  listener_rc: &Rc<AsyncRefCell<Option<tokio::net::TcpListener>>>,
  cancel: &Rc<CancelHandle>,
) -> Result<
  (
    tokio::net::TcpStream,
    std::net::SocketAddr,
    std::net::SocketAddr,
  ),
  std::io::Error,
> {
  let accept_fut = async {
    let mut guard = listener_rc.borrow_mut().await;
    let opt: &mut Option<tokio::net::TcpListener> = &mut *guard;
    match opt {
      Some(listener) => listener.accept().await,
      None => Err(std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "listener closed",
      )),
    }
  };

  let result = accept_fut.or_cancel(cancel.clone()).await;
  let (stream, remote_addr) = result.map_err(|_| {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "listener closed")
  })??;

  let local_addr = stream.local_addr()?;
  Ok((stream, local_addr, remote_addr))
}
