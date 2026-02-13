// Copyright 2018-2026 the Deno authors. MIT license.

use std::cell::Cell;
use std::cell::RefCell;
use std::io::ErrorKind;
use std::rc::Rc;
use std::time::Duration;

use deno_core::CppgcBase;
use deno_core::CppgcInherits;
use deno_core::GarbageCollected;
use deno_core::JsRuntime;
use deno_core::OpState;
use deno_core::op2;
use deno_core::v8;
use deno_permissions::PermissionsContainer;
use tokio::net::TcpStream;

use super::connection_wrap::ConnectionWrap;
use super::handle_wrap::AsyncWrap;
use super::handle_wrap::HandleWrap;
use super::stream_wrap::LibuvStreamWrap;

// `providerType.TCPCONNECTWRAP`
const PROVIDER_TCPCONNECTWRAP: i32 = 32;
// `providerType.TCPSERVERWRAP`
const PROVIDER_TCPSERVERWRAP: i32 = 33;
// `providerType.TCPWRAP`
const PROVIDER_TCPWRAP: i32 = 34;

// Node's TCPWrap::SocketType
const SOCKET: i32 = 0;
const SERVER: i32 = 1;

const UV_EINVAL: i32 = -22;
const UV_EADDRNOTAVAIL: i32 = -99;
const UV_EACCES: i32 = -13;

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(AsyncWrap)]
#[repr(C)]
pub struct TCPConnectWrap {
  base: AsyncWrap,
}

// SAFETY: we're sure this can be GCed
unsafe impl GarbageCollected for TCPConnectWrap {
  fn trace(&self, _visitor: &mut deno_core::v8::cppgc::Visitor) {}

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"TCPConnectWrap"
  }
}

#[op2(base, inherit = AsyncWrap)]
impl TCPConnectWrap {
  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L118-L123
  #[constructor]
  #[cppgc]
  fn new(state: &mut OpState) -> TCPConnectWrap {
    TCPConnectWrap {
      base: AsyncWrap::create(state, PROVIDER_TCPCONNECTWRAP),
    }
  }
}

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(ConnectionWrap)]
#[repr(C)]
pub struct TCP {
  base: ConnectionWrap,
  is_server: Cell<bool>,
  refed: Rc<Cell<bool>>,
  connection_count: Rc<Cell<i32>>,
  parent_connection_count: Option<Rc<Cell<i32>>>,
  reading: Rc<Cell<bool>>,
  fd: Cell<i32>,
  stream: Rc<RefCell<Option<TcpStream>>>,
  listener: Rc<RefCell<Option<std::net::TcpListener>>>,
  accepting: Rc<Cell<bool>>,
  closed: Rc<Cell<bool>>,
  spawner: deno_core::V8TaskSpawner,
  local_address: Rc<RefCell<Option<String>>>,
  local_port: Rc<Cell<Option<u16>>>,
  remote_address: Rc<RefCell<Option<String>>>,
  remote_port: Rc<Cell<Option<u16>>>,
}

// SAFETY: we're sure this can be GCed
unsafe impl GarbageCollected for TCP {
  fn trace(&self, _visitor: &mut deno_core::v8::cppgc::Visitor) {}

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"TCP"
  }
}

impl TCP {
  fn create(
    connection_wrap: ConnectionWrap,
    is_server: bool,
    spawner: deno_core::V8TaskSpawner,
  ) -> Self {
    Self {
      base: connection_wrap,
      is_server: Cell::new(is_server),
      refed: Rc::new(Cell::new(true)),
      connection_count: Rc::new(Cell::new(0)),
      parent_connection_count: None,
      reading: Rc::new(Cell::new(false)),
      fd: Cell::new(-1),
      stream: Rc::new(RefCell::new(None)),
      listener: Rc::new(RefCell::new(None)),
      accepting: Rc::new(Cell::new(false)),
      closed: Rc::new(Cell::new(false)),
      spawner,
      local_address: Rc::new(RefCell::new(None)),
      local_port: Rc::new(Cell::new(None)),
      remote_address: Rc::new(RefCell::new(None)),
      remote_port: Rc::new(Cell::new(None)),
    }
  }

  fn connect_impl(
    &self,
    op_state: Rc<RefCell<OpState>>,
    this: v8::Global<v8::Object>,
    req: v8::Global<v8::Object>,
    address: String,
    port: u16,
  ) -> i32 {
    self.remote_address.replace(Some(address.clone()));
    self.remote_port.set(Some(port));

    let stream_slot = self.stream.clone();
    let local_address = self.local_address.clone();
    let local_port = self.local_port.clone();
    let remote_address = self.remote_address.clone();
    let remote_port = self.remote_port.clone();
    deno_core::unsync::spawn(async move {
      let status = match TcpStream::connect((address.as_str(), port)).await {
        Ok(stream) => {
          if let Ok(local_addr) = stream.local_addr() {
            local_address.replace(Some(local_addr.ip().to_string()));
            local_port.set(Some(local_addr.port()));
          }
          if let Ok(peer_addr) = stream.peer_addr() {
            remote_address.replace(Some(peer_addr.ip().to_string()));
            remote_port.set(Some(peer_addr.port()));
          }
          stream_slot.replace(Some(stream));
          0
        }
        Err(err) => error_to_status(err),
      };

      op_state
        .borrow()
        .borrow::<deno_core::V8TaskSpawner>()
        .spawn(move |scope| call_connect_oncomplete(scope, this, req, status));
    });
    0
  }

  fn start_read_loop(&self, this: v8::Global<v8::Object>) -> i32 {
    if self.reading.get() {
      return 0;
    }
    self.reading.set(true);

    let reading = self.reading.clone();
    let stream = self.stream.clone();
    let this = Rc::new(this);
    let spawner = self.spawner.clone();
    deno_core::unsync::spawn(async move {
      let mut buf = vec![0u8; 64 * 1024];
      while reading.get() {
        let read_result = {
          let stream_ref = stream.borrow();
          let Some(stream) = stream_ref.as_ref() else {
            break;
          };
          stream.try_read(&mut buf)
        };

        match read_result {
          Ok(0) => {
            reading.set(false);
            let this = this.clone();
            spawner.spawn(move |scope| {
              let this_local = v8::Local::new(scope, &*this);
              call_onread(scope, this_local, -4095, Vec::new());
            });
            break;
          }
          Ok(nread) => {
            let this = this.clone();
            let data = buf[..nread].to_vec();
            spawner.spawn(move |scope| {
              let this_local = v8::Local::new(scope, &*this);
              call_onread(scope, this_local, nread as i32, data);
            });
          }
          Err(err) if err.kind() == ErrorKind::WouldBlock => {
            tokio::time::sleep(Duration::from_millis(10)).await;
          }
          Err(err) => {
            reading.set(false);
            let this = this.clone();
            let status = error_to_status(err);
            spawner.spawn(move |scope| {
              let this_local = v8::Local::new(scope, &*this);
              call_onread(scope, this_local, status, Vec::new());
            });
            break;
          }
        }
      }
    });

    0
  }

  fn write_impl(&self, req: v8::Global<v8::Object>, data: Vec<u8>) -> i32 {
    let stream = self.stream.clone();
    let spawner = self.spawner.clone();
    deno_core::unsync::spawn(async move {
      let mut offset = 0usize;
      let status = loop {
        if offset >= data.len() {
          break 0;
        }

        let write_result = {
          let stream_ref = stream.borrow();
          let Some(stream) = stream_ref.as_ref() else {
            break not_connected_status();
          };
          stream.try_write(&data[offset..])
        };

        match write_result {
          Ok(0) => break not_connected_status(),
          Ok(nwritten) => {
            offset += nwritten;
          }
          Err(err) if err.kind() == ErrorKind::WouldBlock => {
            tokio::time::sleep(Duration::from_millis(5)).await;
          }
          Err(err) => break error_to_status(err),
        }
      };

      spawner.spawn(move |scope| {
        call_req_oncomplete(scope, req, status);
      });
    });

    0
  }

  fn shutdown_impl(&self, req: v8::Global<v8::Object>) -> i32 {
    let stream = self.stream.clone();
    let spawner = self.spawner.clone();
    deno_core::unsync::spawn(async move {
      let status = {
        let stream_ref = stream.borrow();
        match stream_ref.as_ref() {
          Some(stream) => match shutdown_stream_write(stream) {
            Ok(()) => 0,
            Err(err) => error_to_status(err),
          },
          None => not_connected_status(),
        }
      };

      spawner.spawn(move |scope| {
        call_req_oncomplete(scope, req, status);
      });
    });

    0
  }

  fn close_underlying(&self) {
    if !self.closed.get()
      && let Some(parent_connection_count) = &self.parent_connection_count
    {
      parent_connection_count.set(
        parent_connection_count.get().saturating_sub(1),
      );
    }
    self.reading.set(false);
    self.accepting.set(false);
    self.closed.set(true);
    self.listener.borrow_mut().take();
    self.stream.borrow_mut().take();
  }

  pub(crate) fn take_stream_for_http(&self) -> Option<TcpStream> {
    self.stream.borrow_mut().take()
  }
}

static ON_CLOSE_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("_onClose");
static ON_COMPLETE_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("oncomplete");
static ON_CONNECTION_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("onconnection");
static ON_READ_STR: deno_core::FastStaticString = deno_core::ascii_str!("onread");
static ADDRESS_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("address");
static FAMILY_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("family");
static PORT_STR: deno_core::FastStaticString = deno_core::ascii_str!("port");

#[op2(base, inherit = ConnectionWrap)]
impl TCP {
  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L154-L179
  #[constructor]
  #[cppgc]
  fn new(#[smi] socket_type: i32, state: &mut OpState) -> TCP {
    let provider = match socket_type {
      SOCKET => PROVIDER_TCPWRAP,
      SERVER => PROVIDER_TCPSERVERWRAP,
      _ => PROVIDER_TCPWRAP,
    };
    let is_server = socket_type == SERVER;
    let spawner = state.borrow::<deno_core::V8TaskSpawner>().clone();

    TCP::create(
      ConnectionWrap::create(LibuvStreamWrap::create(HandleWrap::create(
        AsyncWrap::create(state, provider),
        None,
      ))),
      is_server,
      spawner,
    )
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L224-L239
  #[fast]
  fn open(&self, #[smi] fd: i32) -> i32 {
    if fd < 0 {
      return UV_EINVAL;
    }

    #[cfg(unix)]
    let std_stream = {
      use std::os::fd::FromRawFd;
      // SAFETY: ownership of `fd` is intentionally transferred to this stream.
      let stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
      stream
    };

    #[cfg(windows)]
    let std_stream = {
      use std::os::windows::io::FromRawSocket;
      // SAFETY: ownership of `fd` is intentionally transferred to this stream.
      let stream = unsafe { std::net::TcpStream::from_raw_socket(fd as usize) };
      stream
    };

    if let Err(err) = std_stream.set_nonblocking(true) {
      return error_to_status(err);
    }

    let tokio_stream = match TcpStream::from_std(std_stream) {
      Ok(stream) => stream,
      Err(err) => return error_to_status(err),
    };

    if let Ok(local_addr) = tokio_stream.local_addr() {
      self
        .local_address
        .replace(Some(local_addr.ip().to_string()));
      self.local_port.set(Some(local_addr.port()));
    }
    if let Ok(peer_addr) = tokio_stream.peer_addr() {
      self
        .remote_address
        .replace(Some(peer_addr.ip().to_string()));
      self.remote_port.set(Some(peer_addr.port()));
    }

    self.stream.borrow_mut().replace(tokio_stream);
    self.fd.set(fd);
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L275-L283
  #[fast]
  fn bind(&self, #[string] address: &str, #[smi] port: i32) -> i32 {
    if !(0..=u16::MAX as i32).contains(&port) {
      return UV_EINVAL;
    }
    self.local_address.replace(Some(address.to_string()));
    self.local_port.set(Some(port as u16));
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L280-L283
  fn bind6(
    &self,
    #[string] address: &str,
    #[smi] port: i32,
    #[smi] _flags: Option<i32>,
  ) -> i32 {
    if !(0..=u16::MAX as i32).contains(&port) {
      return UV_EINVAL;
    }
    self.local_address.replace(Some(address.to_string()));
    self.local_port.set(Some(port as u16));
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L285-L300
  #[fast]
  fn listen(&self, #[smi] backlog: i32) -> i32 {
    if backlog < 0 || !self.is_server.get() {
      return UV_EINVAL;
    }

    let Some(address) = self.local_address.borrow().clone() else {
      return UV_EADDRNOTAVAIL;
    };
    let Some(port) = self.local_port.get() else {
      return UV_EADDRNOTAVAIL;
    };
    let std_listener =
      match std::net::TcpListener::bind((address.as_str(), port)) {
        Ok(listener) => listener,
        Err(err) => return error_to_status(err),
      };
    if let Err(err) = std_listener.set_nonblocking(true) {
      return error_to_status(err);
    }
    if let Ok(local_addr) = std_listener.local_addr() {
      self
        .local_address
        .replace(Some(local_addr.ip().to_string()));
      self.local_port.set(Some(local_addr.port()));
    }

    self.listener.borrow_mut().replace(std_listener);
    self.closed.set(false);
    self.accepting.set(false);

    0
  }

  #[fast]
  #[reentrant]
  fn read_start(&self, #[this] this: v8::Global<v8::Object>) -> i32 {
    self.start_read_loop(this)
  }

  #[fast]
  fn read_stop(&self) -> i32 {
    self.reading.set(false);
    0
  }

  #[fast]
  #[reentrant]
  fn read_start_native(&self, #[this] this: v8::Global<v8::Object>) -> i32 {
    self.start_read_loop(this)
  }

  #[fast]
  fn read_stop_native(&self) -> i32 {
    self.reading.set(false);
    0
  }

  #[reentrant]
  fn write_buffer(
    &self,
    #[scoped] req: v8::Global<v8::Object>,
    #[buffer] data: &[u8],
  ) -> i32 {
    self.write_impl(req, data.to_vec())
  }

  #[reentrant]
  fn write_ascii_string(
    &self,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] data: &str,
  ) -> i32 {
    self.write_impl(req, data.as_bytes().to_vec())
  }

  #[reentrant]
  fn write_utf8_string(
    &self,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] data: &str,
  ) -> i32 {
    self.write_impl(req, data.as_bytes().to_vec())
  }

  #[reentrant]
  fn write_ucs2_string(
    &self,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] data: &str,
  ) -> i32 {
    let mut out = Vec::with_capacity(data.len().saturating_mul(2));
    for unit in data.encode_utf16() {
      out.push((unit & 0x00ff) as u8);
      out.push((unit >> 8) as u8);
    }
    self.write_impl(req, out)
  }

  #[reentrant]
  fn write_latin1_string(
    &self,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] data: &str,
  ) -> i32 {
    let out = data.chars().map(|ch| (ch as u32 & 0xff) as u8).collect();
    self.write_impl(req, out)
  }

  #[reentrant]
  fn shutdown(&self, #[scoped] req: v8::Global<v8::Object>) -> i32 {
    self.shutdown_impl(req)
  }

  #[reentrant]
  fn shutdown_native(&self, #[scoped] req: v8::Global<v8::Object>) -> i32 {
    self.shutdown_impl(req)
  }

  #[fast]
  #[rename("ref")]
  fn ref_method(&self) {
    self.refed.set(true);
  }

  #[fast]
  fn unref(&self) {
    self.refed.set(false);
  }

  #[fast]
  fn mark_unrefed(&self) {
    self.refed.set(false);
  }

  #[fast]
  fn mark_refed(&self) {
    self.refed.set(true);
  }

  #[fast]
  #[reentrant]
  fn start_listen(
    &self,
    op_state: Rc<RefCell<OpState>>,
    scope: &mut v8::PinScope<'_, '_>,
    #[this] this: v8::Global<v8::Object>,
  ) -> i32 {
    let _ = scope;
    if !self.is_server.get() {
      return UV_EINVAL;
    }
    if self.accepting.get() {
      return 0;
    }

    let Some(address) = self.local_address.borrow().clone() else {
      return UV_EADDRNOTAVAIL;
    };
    let Some(port) = self.local_port.get() else {
      return UV_EADDRNOTAVAIL;
    };
    if op_state
      .borrow_mut()
      .borrow_mut::<PermissionsContainer>()
      .check_net(&(&address, Some(port)), "TCP.listen")
      .is_err()
    {
      return UV_EACCES;
    }

    self.accepting.set(true);
    let this = Rc::new(this);
    let accepting = self.accepting.clone();
    let refed = self.refed.clone();
    let connection_count = self.connection_count.clone();
    let closed = self.closed.clone();
    let listener = self.listener.clone();
    deno_core::unsync::spawn(async move {
      let mut unref_idle_polls = 0u32;
      while accepting.get() && !closed.get() {
        let result = {
          let listener_ref = listener.borrow();
          let Some(listener) = listener_ref.as_ref() else {
            break;
          };
          listener.accept()
        };

        match result {
          Ok((stream, _)) => {
            unref_idle_polls = 0;
            if let Err(err) = stream.set_nonblocking(true) {
              let status = error_to_status(err);
              let this = this.clone();
              op_state
                .borrow()
                .borrow::<deno_core::V8TaskSpawner>()
                .spawn(move |scope| {
                  let this_local = v8::Local::new(scope, &*this);
                  call_onconnection(scope, this_local, status, None);
              });
              tokio::time::sleep(Duration::from_millis(20)).await;
              continue;
            }
            let stream = match TcpStream::from_std(stream) {
              Ok(stream) => stream,
              Err(err) => {
                let status = error_to_status(err);
                let this = this.clone();
                op_state
                  .borrow()
                  .borrow::<deno_core::V8TaskSpawner>()
                  .spawn(move |scope| {
                    let this_local = v8::Local::new(scope, &*this);
                    call_onconnection(scope, this_local, status, None);
                  });
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
              }
            };
            let this = this.clone();
            let parent_connection_count = connection_count.clone();
            connection_count
              .set(connection_count.get().saturating_add(1));
            op_state
              .borrow()
              .borrow::<deno_core::V8TaskSpawner>()
              .spawn(move |scope| {
                let client_obj = {
                  let op_state = JsRuntime::op_state_from(scope);
                  let mut op_state = op_state.borrow_mut();
                  let spawner =
                    op_state.borrow::<deno_core::V8TaskSpawner>().clone();

                  let mut client = TCP::create(
                    ConnectionWrap::create(LibuvStreamWrap::create(
                      HandleWrap::create(
                        AsyncWrap::create(&mut op_state, PROVIDER_TCPWRAP),
                        None,
                      ),
                    )),
                    false,
                    spawner,
                  );
                  client.parent_connection_count =
                    Some(parent_connection_count.clone());
                  if let Ok(local_addr) = stream.local_addr() {
                    client
                      .local_address
                      .replace(Some(local_addr.ip().to_string()));
                    client.local_port.set(Some(local_addr.port()));
                  }
                  if let Ok(peer_addr) = stream.peer_addr() {
                    client
                      .remote_address
                      .replace(Some(peer_addr.ip().to_string()));
                    client.remote_port.set(Some(peer_addr.port()));
                  }
                  client.stream.borrow_mut().replace(stream);

                  deno_core::cppgc::make_cppgc_object(scope, client)
                };
                let this_local = v8::Local::new(scope, &*this);
                call_onconnection(scope, this_local, 0, Some(client_obj));
              });
          }
          Err(err) if err.kind() == ErrorKind::WouldBlock => {
            if refed.get() {
              unref_idle_polls = 0;
            } else {
              unref_idle_polls = unref_idle_polls.saturating_add(1);
              if unref_idle_polls >= 100 {
                accepting.set(false);
                break;
              }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
          }
          Err(err) => {
            let status = error_to_status(err);
            let this = this.clone();
            op_state
              .borrow()
              .borrow::<deno_core::V8TaskSpawner>()
              .spawn(move |scope| {
                let this_local = v8::Local::new(scope, &*this);
                call_onconnection(scope, this_local, status, None);
              });
            if refed.get() {
              unref_idle_polls = 0;
            } else {
              unref_idle_polls = unref_idle_polls.saturating_add(1);
              if unref_idle_polls >= 50 {
                accepting.set(false);
                break;
              }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
          }
        }
      }
    });

    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L302-L373
  #[reentrant]
  fn connect(
    &self,
    op_state: Rc<RefCell<OpState>>,
    #[this] this: v8::Global<v8::Object>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] address: &str,
    #[smi] port: i32,
  ) -> i32 {
    if !(0..=u16::MAX as i32).contains(&port) {
      return UV_EINVAL;
    }
    if op_state
      .borrow_mut()
      .borrow_mut::<PermissionsContainer>()
      .check_net(&(address, Some(port as u16)), "TCP.connect")
      .is_err()
    {
      return UV_EACCES;
    }

    self.connect_impl(op_state, this, req, address.to_string(), port as u16)
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L313-L373
  #[reentrant]
  fn connect6(
    &self,
    op_state: Rc<RefCell<OpState>>,
    #[this] this: v8::Global<v8::Object>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] address: &str,
    #[smi] port: i32,
  ) -> i32 {
    if !(0..=u16::MAX as i32).contains(&port) {
      return UV_EINVAL;
    }
    if op_state
      .borrow_mut()
      .borrow_mut::<PermissionsContainer>()
      .check_net(&(address, Some(port as u16)), "TCP.connect6")
      .is_err()
    {
      return UV_EACCES;
    }

    self.connect_impl(op_state, this, req, address.to_string(), port as u16)
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L102-L107
  #[reentrant]
  fn getsockname(
    &self,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] out: v8::Global<v8::Object>,
  ) -> i32 {
    let Some(address) = self.local_address.borrow().clone() else {
      return UV_EADDRNOTAVAIL;
    };
    let Some(port) = self.local_port.get() else {
      return UV_EADDRNOTAVAIL;
    };

    let out = v8::Local::new(scope, out);
    write_address_info(scope, out, &address, port);
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L106-L107
  #[reentrant]
  fn getpeername(
    &self,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] out: v8::Global<v8::Object>,
  ) -> i32 {
    let Some(address) = self.remote_address.borrow().clone() else {
      return UV_EADDRNOTAVAIL;
    };
    let Some(port) = self.remote_port.get() else {
      return UV_EADDRNOTAVAIL;
    };

    let out = v8::Local::new(scope, out);
    write_address_info(scope, out, &address, port);
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L189-L197
  #[fast]
  fn set_no_delay(&self, enable: bool) -> i32 {
    if let Some(stream) = self.stream.borrow().as_ref()
      && let Err(err) = stream.set_nodelay(enable)
    {
      return error_to_status(err);
    }
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L199-L211
  #[fast]
  fn set_keep_alive(&self, _enable: i32, _delay: u32) -> i32 {
    0
  }

  // Ported from Node.js (Windows-only in Node core).
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L213-L222
  #[fast]
  fn set_simultaneous_accepts(&self, _enable: bool) -> i32 {
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/tcp_wrap.cc#L373-L400
  #[reentrant]
  fn reset(
    &self,
    scope: &mut v8::PinScope<'_, '_>,
    #[this] this: v8::Global<v8::Object>,
    #[scoped] close_callback: Option<v8::Global<v8::Function>>,
  ) -> i32 {
    self.close_underlying();
    let this_local = v8::Local::new(scope, this);
    let on_close_key = ON_CLOSE_STR.v8_string(scope).unwrap();

    if let Some(on_close) = this_local
      .get(scope, on_close_key.into())
      .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok())
    {
      on_close.call(scope, this_local.into(), &[]);
    }

    if let Some(close_callback) = close_callback {
      let recv = v8::undefined(scope);
      close_callback.open(scope).call(scope, recv.into(), &[]);
    }

    0
  }

  #[getter]
  fn fd(&self) -> i32 {
    self.fd.get()
  }

  #[fast]
  #[rename("_onClose")]
  fn on_close(&self) -> i32 {
    self.close_underlying();
    0
  }
}

fn error_to_status(err: std::io::Error) -> i32 {
  err.raw_os_error().map(|code| -code).unwrap_or(-1)
}

fn not_connected_status() -> i32 {
  std::io::Error::from(ErrorKind::NotConnected)
    .raw_os_error()
    .map(|code| -code)
    .unwrap_or(UV_EINVAL)
}

#[cfg(unix)]
fn shutdown_stream_write(stream: &TcpStream) -> std::io::Result<()> {
  use std::os::fd::AsRawFd;
  let fd = stream.as_raw_fd();
  // SAFETY: `fd` is a valid socket descriptor owned by `stream`.
  let rc = unsafe { libc::shutdown(fd, libc::SHUT_WR) };
  if rc == 0 {
    Ok(())
  } else {
    Err(std::io::Error::last_os_error())
  }
}

#[cfg(not(unix))]
fn shutdown_stream_write(_stream: &TcpStream) -> std::io::Result<()> {
  Ok(())
}

fn write_address_info(
  scope: &mut v8::PinScope<'_, '_>,
  out: v8::Local<v8::Object>,
  address: &str,
  port: u16,
) {
  let family = if address.contains(':') {
    "IPv6"
  } else {
    "IPv4"
  };

  let address_key = ADDRESS_STR.v8_string(scope).unwrap();
  let family_key = FAMILY_STR.v8_string(scope).unwrap();
  let port_key = PORT_STR.v8_string(scope).unwrap();

  let address = v8::String::new(scope, address).unwrap();
  let family = v8::String::new(scope, family).unwrap();
  let port = v8::Integer::new_from_unsigned(scope, port as u32);

  out.set(scope, address_key.into(), address.into());
  out.set(scope, family_key.into(), family.into());
  out.set(scope, port_key.into(), port.into());
}

fn call_connect_oncomplete(
  scope: &mut v8::PinScope<'_, '_>,
  this: v8::Global<v8::Object>,
  req: v8::Global<v8::Object>,
  status: i32,
) {
  let this = v8::Local::new(scope, this);
  let req = v8::Local::new(scope, req);
  let on_complete_key = ON_COMPLETE_STR.v8_string(scope).unwrap();

  let on_complete = req.get(scope, on_complete_key.into());
  let Some(on_complete) = on_complete else {
    return;
  };
  let Ok(on_complete) = v8::Local::<v8::Function>::try_from(on_complete) else {
    return;
  };

  let success = status == 0;
  let status = v8::Integer::new(scope, status);
  let readable = v8::Boolean::new(scope, success);
  let writable = v8::Boolean::new(scope, success);
  on_complete.call(
    scope,
    req.into(),
    &[
      status.into(),
      this.into(),
      req.into(),
      readable.into(),
      writable.into(),
    ],
  );
}

fn call_req_oncomplete(
  scope: &mut v8::PinScope<'_, '_>,
  req: v8::Global<v8::Object>,
  status: i32,
) {
  let req = v8::Local::new(scope, req);
  let on_complete_key = ON_COMPLETE_STR.v8_string(scope).unwrap();
  let on_complete = req.get(scope, on_complete_key.into());
  let Some(on_complete) = on_complete else {
    return;
  };
  let Ok(on_complete) = v8::Local::<v8::Function>::try_from(on_complete) else {
    return;
  };

  let status = v8::Integer::new(scope, status);
  on_complete.call(scope, req.into(), &[status.into()]);
}

fn call_onread(
  scope: &mut v8::PinScope<'_, '_>,
  this: v8::Local<v8::Object>,
  nread: i32,
  data: Vec<u8>,
) {
  let on_read_key = ON_READ_STR.v8_string(scope).unwrap();
  let on_read = this.get(scope, on_read_key.into());
  let Some(on_read) = on_read else {
    return;
  };
  let Ok(on_read) = v8::Local::<v8::Function>::try_from(on_read) else {
    return;
  };

  let len = data.len();
  let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(data);
  let array_buffer =
    v8::ArrayBuffer::with_backing_store(scope, &backing_store.make_shared());
  let uint8 =
    v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();
  let nread = v8::Integer::new(scope, nread);
  on_read.call(scope, this.into(), &[uint8.into(), nread.into()]);
}

fn call_onconnection(
  scope: &mut v8::PinScope<'_, '_>,
  this: v8::Local<v8::Object>,
  status: i32,
  client: Option<v8::Local<v8::Object>>,
) {
  let on_connection_key = ON_CONNECTION_STR.v8_string(scope).unwrap();
  let on_connection = this.get(scope, on_connection_key.into());
  let Some(on_connection) = on_connection else {
    return;
  };
  let Ok(on_connection) = v8::Local::<v8::Function>::try_from(on_connection)
  else {
    return;
  };

  let status = v8::Integer::new(scope, status);
  let client = client
    .map(|c| c.into())
    .unwrap_or_else(|| v8::undefined(scope).into());
  on_connection.call(scope, this.into(), &[status.into(), client]);
}

#[cfg(test)]
mod tests {
  use std::future::poll_fn;
  use std::task::Poll;

  use deno_core::JsRuntime;
  use deno_core::RuntimeOptions;

  async fn js_test(source_code: &'static str) {
    deno_core::extension!(
      test_ext,
      objects = [
        super::AsyncWrap,
        super::HandleWrap,
        super::LibuvStreamWrap,
        super::ConnectionWrap,
        super::TCPConnectWrap,
        super::TCP,
      ],
      state = |state| {
        state.put::<super::super::handle_wrap::AsyncId>(
          super::super::handle_wrap::AsyncId::default(),
        );
        state.put::<super::super::stream_wrap::StreamBaseState>(
          super::super::stream_wrap::StreamBaseState::default(),
        );
      }
    );

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![test_ext::init()],
      ..Default::default()
    });

    poll_fn(move |cx| {
      runtime
        .execute_script("file://tcp_wrap_test.js", source_code)
        .unwrap();

      let result = runtime.poll_event_loop(cx, Default::default());
      assert!(matches!(result, Poll::Ready(Ok(()))));
      Poll::Ready(())
    })
    .await;
  }

  #[tokio::test(flavor = "current_thread")]
  async fn test_tcp_wrap_connect_and_names() {
    js_test(
      r#"
        const { TCP, TCPConnectWrap } = Deno.core.ops;

        const tcp = new TCP(0);
        if (tcp.bind("127.0.0.1", 8000) !== 0) {
          throw new Error("bind should succeed");
        }

        const req = new TCPConnectWrap();
        let completed = false;
        req.oncomplete = (status, handle, reqObj, readable, writable) => {
          completed = true;
          if (status !== 0 || handle !== tcp || reqObj !== req) {
            throw new Error("connect callback args are wrong");
          }
          if (!readable || !writable) {
            throw new Error("connect callback flags should be true on success");
          }
        };

        if (tcp.connect(req, "127.0.0.1", 443) !== 0) {
          throw new Error("connect should return 0");
        }

        const sock = {};
        if (tcp.getsockname(sock) !== 0 || sock.address !== "127.0.0.1") {
          throw new Error("getsockname should populate local address");
        }
        const peer = {};
        if (tcp.getpeername(peer) !== 0 || peer.address !== "127.0.0.1") {
          throw new Error("getpeername should populate remote address");
        }
      "#,
    )
    .await;
  }
}
