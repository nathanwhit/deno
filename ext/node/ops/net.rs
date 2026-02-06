use std::borrow::Cow;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::c_void;
use std::io::Error;
use std::io::ErrorKind;
use std::mem;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::rc::Rc;

use deno_core::AsyncRefCell;
use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::op2;
use deno_error::JsErrorBox;
use deno_net::ops::IpAddr;
use deno_permissions::PermissionsContainer;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use tokio::io::Interest;
#[cfg(unix)]
use tokio::io::unix::AsyncFd;

#[cfg(unix)]
/// Newtype wrapper around a raw fd that does NOT close on drop.
/// Used to register libuv's backend fd with tokio's reactor without
/// transferring ownership.
struct BackendFd(RawFd);

#[cfg(unix)]
impl AsRawFd for BackendFd {
  fn as_raw_fd(&self) -> RawFd {
    self.0
  }
}

fn uv_status_description(status: i32) -> String {
  // SAFETY: libuv returns static strings for error names and messages.
  unsafe {
    let name = CStr::from_ptr(deno_libuv::sys::uv_err_name(status))
      .to_string_lossy()
      .into_owned();
    let message = CStr::from_ptr(deno_libuv::sys::uv_strerror(status))
      .to_string_lossy()
      .into_owned();
    format!("{name}: {message} ({status})")
  }
}

fn uv_status_to_js_error(status: i32, context: &str) -> JsErrorBox {
  JsErrorBox::generic(format!("{context}: {}", uv_status_description(status)))
}

fn uv_status_to_io_error(status: i32, context: &str) -> std::io::Error {
  std::io::Error::other(format!("{context}: {}", uv_status_description(status)))
}

#[derive(Clone)]
pub struct LibUvLoop {
  libuv_loop: Rc<deno_libuv::UvLoop>,
  driver_running: Rc<Cell<bool>>,
}

pub struct LibUvLoopDriver {
  #[cfg(unix)]
  async_fd: AsyncFd<BackendFd>,
  libuv_loop: Rc<deno_libuv::UvLoop>,
}

impl LibUvLoopDriver {
  pub fn new(libuv_loop: Rc<deno_libuv::UvLoop>) -> Self {
    #[cfg(unix)]
    {
      let fd = BackendFd(unsafe {
        deno_libuv::sys::uv_backend_fd(libuv_loop.as_ptr())
      });
      let async_fd = AsyncFd::with_interest(fd, Interest::READABLE).unwrap();
      Self {
        libuv_loop,
        async_fd,
      }
    }

    #[cfg(not(unix))]
    {
      Self { libuv_loop }
    }
  }

  pub async fn drive(&mut self) {
    loop {
      let _ = self.libuv_loop.run(deno_libuv::RunMode::NoWait);
      if !self.libuv_loop.alive() {
        break;
      }

      #[cfg(unix)]
      {
        if let Ok(mut guard) = self.async_fd.readable().await {
          guard.clear_ready();
        } else {
          tokio::task::yield_now().await;
        }
      }

      #[cfg(not(unix))]
      {
        // TODO: poll the handle or whatever on windows, idk if that's a thing
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
      }
    }
  }
}

impl LibUvLoop {
  pub fn new() -> Self {
    let libuv_loop =
      deno_libuv::UvLoop::new().expect("Failed to create libuv loop");

    LibUvLoop {
      libuv_loop,
      driver_running: Rc::new(Cell::new(false)),
    }
  }

  pub fn ensure_driver(&self) {
    if self.driver_running.get() {
      return;
    }

    self.driver_running.set(true);

    deno_core::unsync::spawn({
      let libuv_loop = self.libuv_loop.clone();
      let driver_running = self.driver_running.clone();
      async move {
        let mut driver = LibUvLoopDriver::new(libuv_loop);
        driver.drive().await;
        driver_running.set(false);
      }
    });
  }

  pub fn get_or_create(op_state: Rc<RefCell<OpState>>) -> Self {
    if let Some(libuv_loop) = op_state.borrow().try_borrow::<LibUvLoop>() {
      libuv_loop.clone()
    } else {
      let libuv_loop = LibUvLoop::new();
      op_state.borrow_mut().put(libuv_loop.clone());
      libuv_loop
    }
  }

  pub fn get_or_create_mut(state: &mut OpState) -> Self {
    if let Some(libuv_loop) = state.try_borrow::<LibUvLoop>() {
      libuv_loop.clone()
    } else {
      let libuv_loop = LibUvLoop::new();
      state.put(libuv_loop.clone());
      libuv_loop
    }
  }
}

enum ReadEvent {
  Data(Vec<u8>),
  Eof,
  Error(i32),
}

struct UvTcpHandleData {
  read_tx: mpsc::UnboundedSender<ReadEvent>,
  /// Cached buffer reused across alloc_cb/read_cb cycles to avoid
  /// allocating a fresh ~64 KB Vec on every read.
  read_buf: Option<Vec<u8>>,
}

enum AcceptEvent {
  Connection(*mut deno_libuv::sys::uv_tcp_t),
  Error(i32),
  Closed,
}

struct UvTcpListenerHandleData {
  accept_tx: mpsc::UnboundedSender<AcceptEvent>,
}

struct ConnectReqData {
  tx: Option<oneshot::Sender<i32>>,
}

struct WriteReqData {
  tx: Option<oneshot::Sender<i32>>,
  payload: Box<[u8]>,
}

unsafe extern "C" fn uv_tcp_alloc_cb(
  handle: *mut deno_libuv::sys::uv_handle_t,
  suggested_size: usize,
  buf: *mut deno_libuv::sys::uv_buf_t,
) {
  let size = suggested_size.max(1);

  // Reuse the cached buffer from handle data when available to avoid
  // allocating a fresh Vec on every read.
  // SAFETY: handle and its data pointer are valid; set by us.
  let data_ptr = unsafe { deno_libuv::sys::uv_handle_get_data(handle) };
  let mut bytes = if !data_ptr.is_null() {
    let handle_data = unsafe { &mut *(data_ptr as *mut UvTcpHandleData) };
    handle_data
      .read_buf
      .take()
      .unwrap_or_else(|| Vec::with_capacity(size))
  } else {
    Vec::with_capacity(size)
  };

  if bytes.capacity() < size {
    bytes.reserve(size - bytes.capacity());
  }

  let base = bytes.as_mut_ptr();
  let len = bytes.capacity();
  mem::forget(bytes);

  // SAFETY: `buf` is a valid out parameter and libuv takes ownership
  // of `base` until `uv_tcp_read_cb` frees it.
  unsafe {
    *buf = deno_libuv::sys::uv_buf_init(base.cast(), len as u32);
  }
}

unsafe extern "C" fn uv_tcp_read_cb(
  stream: *mut deno_libuv::sys::uv_stream_t,
  nread: isize,
  buf: *const deno_libuv::sys::uv_buf_t,
) {
  let mut allocated = None;
  if !buf.is_null() {
    // SAFETY: `buf` and its `base` were allocated in `uv_tcp_alloc_cb`.
    let (base, len) = unsafe { ((*buf).base, (*buf).len) };
    if !base.is_null() && len > 0 {
      // SAFETY: Reclaim the allocation made in alloc_cb. Length is set
      // to 0 because the bytes are not yet initialized at this point.
      let bytes = unsafe { Vec::from_raw_parts(base.cast::<u8>(), 0, len) };
      allocated = Some(bytes);
    }
  }

  // SAFETY: `stream` is a valid handle and data was set by us.
  let data_ptr = unsafe {
    deno_libuv::sys::uv_handle_get_data(
      stream.cast::<deno_libuv::sys::uv_handle_t>(),
    )
  };
  if data_ptr.is_null() {
    return;
  }

  // SAFETY: pointer comes from Box<UvTcpHandleData>.
  let handle_data = unsafe { &mut *(data_ptr as *mut UvTcpHandleData) };

  if nread > 0 {
    if let Some(alloc_buf) = allocated {
      // Copy only the bytes that were actually read into a right-sized
      // Vec, then return the large buffer for reuse in the next alloc_cb.
      let n = (nread as usize).min(alloc_buf.capacity());
      // SAFETY: libuv guarantees that `n` bytes at the start of the
      // buffer are properly initialized.
      let data =
        unsafe { std::slice::from_raw_parts(alloc_buf.as_ptr(), n) }.to_vec();
      handle_data.read_buf = Some(alloc_buf);
      let _ = handle_data.read_tx.send(ReadEvent::Data(data));
    }
    return;
  }

  // nread <= 0: return the buffer for reuse.
  if let Some(alloc_buf) = allocated {
    handle_data.read_buf = Some(alloc_buf);
  }

  if nread == 0 {
    return;
  }

  if nread == deno_libuv::sys::uv_errno_t_UV_EOF as isize {
    let _ = handle_data.read_tx.send(ReadEvent::Eof);
  } else {
    let _ = handle_data.read_tx.send(ReadEvent::Error(nread as i32));
  }

  // SAFETY: stream is valid.
  unsafe {
    let _ = deno_libuv::sys::uv_read_stop(stream);
  }
}

unsafe extern "C" fn uv_tcp_connect_cb(
  req: *mut deno_libuv::sys::uv_connect_t,
  status: i32,
) {
  // SAFETY: req data was set to Box<ConnectReqData>.
  let data_ptr = unsafe {
    deno_libuv::sys::uv_req_get_data(req.cast::<deno_libuv::sys::uv_req_t>())
  };
  if !data_ptr.is_null() {
    // SAFETY: ownership is transferred back here exactly once.
    let mut data = unsafe { Box::from_raw(data_ptr as *mut ConnectReqData) };
    if let Some(tx) = data.tx.take() {
      let _ = tx.send(status);
    }
  }

  // SAFETY: req allocation was created with Box::into_raw.
  unsafe {
    drop(Box::from_raw(req));
  }
}

unsafe extern "C" fn uv_tcp_write_cb(
  req: *mut deno_libuv::sys::uv_write_t,
  status: i32,
) {
  // SAFETY: req data was set to Box<WriteReqData>.
  let data_ptr = unsafe {
    deno_libuv::sys::uv_req_get_data(req.cast::<deno_libuv::sys::uv_req_t>())
  };
  if !data_ptr.is_null() {
    // SAFETY: ownership is transferred back here exactly once.
    let mut data = unsafe { Box::from_raw(data_ptr as *mut WriteReqData) };
    if let Some(tx) = data.tx.take() {
      let _ = tx.send(status);
    }
  }

  // SAFETY: req allocation was created with Box::into_raw.
  unsafe {
    drop(Box::from_raw(req));
  }
}

unsafe extern "C" fn uv_tcp_close_cb(
  handle: *mut deno_libuv::sys::uv_handle_t,
) {
  // SAFETY: handle data was set to Box<UvTcpHandleData>.
  let data_ptr = unsafe { deno_libuv::sys::uv_handle_get_data(handle) };
  if !data_ptr.is_null() {
    // SAFETY: ownership is transferred back here exactly once.
    unsafe {
      drop(Box::from_raw(data_ptr as *mut UvTcpHandleData));
      deno_libuv::sys::uv_handle_set_data(handle, std::ptr::null_mut());
    }
  }

  // SAFETY: handle allocation was created with Box::into_raw as uv_tcp_t.
  unsafe {
    drop(Box::from_raw(handle.cast::<deno_libuv::sys::uv_tcp_t>()));
  }
}

unsafe extern "C" fn uv_tcp_listener_close_cb(
  handle: *mut deno_libuv::sys::uv_handle_t,
) {
  // SAFETY: handle data was set to Box<UvTcpListenerHandleData>.
  let data_ptr = unsafe { deno_libuv::sys::uv_handle_get_data(handle) };
  if !data_ptr.is_null() {
    unsafe {
      drop(Box::from_raw(data_ptr as *mut UvTcpListenerHandleData));
      deno_libuv::sys::uv_handle_set_data(handle, std::ptr::null_mut());
    }
  }

  // SAFETY: handle allocation was created with Box::into_raw as uv_tcp_t.
  unsafe {
    drop(Box::from_raw(handle.cast::<deno_libuv::sys::uv_tcp_t>()));
  }
}

unsafe extern "C" fn uv_tcp_server_connection_cb(
  server: *mut deno_libuv::sys::uv_stream_t,
  status: i32,
) {
  let data_ptr = unsafe {
    deno_libuv::sys::uv_handle_get_data(
      server.cast::<deno_libuv::sys::uv_handle_t>(),
    )
  };
  if data_ptr.is_null() {
    return;
  }
  let handle_data = unsafe { &*(data_ptr as *const UvTcpListenerHandleData) };

  if status < 0 {
    let _ = handle_data.accept_tx.send(AcceptEvent::Error(status));
    return;
  }

  let client = Box::into_raw(Box::new(unsafe {
    mem::zeroed::<deno_libuv::sys::uv_tcp_t>()
  }));

  let init_rc =
    unsafe { deno_libuv::sys::uv_tcp_init((*server).loop_, client) };
  if init_rc < 0 {
    unsafe {
      drop(Box::from_raw(client));
    }
    let _ = handle_data.accept_tx.send(AcceptEvent::Error(init_rc));
    return;
  }

  let accept_rc = unsafe { deno_libuv::sys::uv_accept(server, client.cast()) };
  if accept_rc < 0 {
    unsafe {
      if deno_libuv::sys::uv_is_closing(client.cast()) == 0 {
        deno_libuv::sys::uv_close(client.cast(), Some(uv_tcp_close_cb));
      }
    }
    let _ = handle_data.accept_tx.send(AcceptEvent::Error(accept_rc));
    return;
  }

  if handle_data
    .accept_tx
    .send(AcceptEvent::Connection(client))
    .is_err()
  {
    // Receiver dropped; close the accepted handle to avoid leaking it.
    unsafe {
      deno_libuv::sys::uv_close(client.cast(), Some(uv_tcp_close_cb));
    }
  }
}

fn sockaddr_to_ip_addr(
  addr: &deno_libuv::sys::sockaddr_storage,
) -> Result<IpAddr, JsErrorBox> {
  let family = i32::from(addr.ss_family);

  if family == libc::AF_INET {
    let addr_v4 = addr as *const _ as *const deno_libuv::sys::sockaddr_in;
    let mut ip_buf = [0i8; 64];

    // SAFETY: `addr_v4` points to a valid IPv4 sockaddr.
    let rc = unsafe {
      deno_libuv::sys::uv_ip4_name(addr_v4, ip_buf.as_mut_ptr(), ip_buf.len())
    };
    if rc < 0 {
      return Err(uv_status_to_js_error(
        rc,
        "failed to decode local IPv4 address",
      ));
    }

    // SAFETY: uv_ip4_name always null-terminates on success.
    let hostname = unsafe { CStr::from_ptr(ip_buf.as_ptr()) }
      .to_string_lossy()
      .into_owned();

    // SAFETY: cast is valid for AF_INET.
    let port = unsafe { u16::from_be((*addr_v4).sin_port) };

    return Ok(IpAddr { hostname, port });
  }

  if family == libc::AF_INET6 {
    let addr_v6 = addr as *const _ as *const deno_libuv::sys::sockaddr_in6;
    let mut ip_buf = [0i8; 64];

    // SAFETY: `addr_v6` points to a valid IPv6 sockaddr.
    let rc = unsafe {
      deno_libuv::sys::uv_ip6_name(addr_v6, ip_buf.as_mut_ptr(), ip_buf.len())
    };
    if rc < 0 {
      return Err(uv_status_to_js_error(
        rc,
        "failed to decode local IPv6 address",
      ));
    }

    // SAFETY: uv_ip6_name always null-terminates on success.
    let hostname = unsafe { CStr::from_ptr(ip_buf.as_ptr()) }
      .to_string_lossy()
      .into_owned();

    // SAFETY: cast is valid for AF_INET6.
    let port = unsafe { u16::from_be((*addr_v6).sin6_port) };

    return Ok(IpAddr { hostname, port });
  }

  Err(JsErrorBox::generic(format!(
    "unsupported address family: {family}"
  )))
}

fn get_tcp_address(
  tcp: *const deno_libuv::sys::uv_tcp_t,
  peer: bool,
) -> Result<IpAddr, JsErrorBox> {
  // SAFETY: zeroed sockaddr storage is valid initialization.
  let mut storage =
    unsafe { mem::zeroed::<deno_libuv::sys::sockaddr_storage>() };
  let mut len = mem::size_of::<deno_libuv::sys::sockaddr_storage>() as i32;

  // SAFETY: pointers are valid and length is initialized.
  let rc = unsafe {
    if peer {
      deno_libuv::sys::uv_tcp_getpeername(
        tcp,
        (&mut storage as *mut deno_libuv::sys::sockaddr_storage)
          .cast::<deno_libuv::sys::sockaddr>(),
        &mut len,
      )
    } else {
      deno_libuv::sys::uv_tcp_getsockname(
        tcp,
        (&mut storage as *mut deno_libuv::sys::sockaddr_storage)
          .cast::<deno_libuv::sys::sockaddr>(),
        &mut len,
      )
    }
  };

  if rc < 0 {
    return Err(uv_status_to_js_error(
      rc,
      "failed to query tcp socket address",
    ));
  }

  sockaddr_to_ip_addr(&storage)
}

fn close_uv_tcp_handle(
  libuv_loop: &LibUvLoop,
  tcp: *mut deno_libuv::sys::uv_tcp_t,
) {
  // SAFETY: `tcp` is a valid handle created by us.
  unsafe {
    if deno_libuv::sys::uv_is_closing(tcp.cast()) == 0 {
      deno_libuv::sys::uv_close(tcp.cast(), Some(uv_tcp_close_cb));
    }
  }
  libuv_loop.ensure_driver();
}

fn close_uv_tcp_listener_handle(
  libuv_loop: &LibUvLoop,
  listener: *mut deno_libuv::sys::uv_tcp_t,
) {
  // SAFETY: `listener` is a valid handle created by us.
  unsafe {
    if deno_libuv::sys::uv_is_closing(listener.cast()) == 0 {
      deno_libuv::sys::uv_close(
        listener.cast(),
        Some(uv_tcp_listener_close_cb),
      );
    }
  }
  libuv_loop.ensure_driver();
}

fn setup_uv_tcp_stream_resource(
  libuv_loop: LibUvLoop,
  tcp: *mut deno_libuv::sys::uv_tcp_t,
) -> Result<UvTcpStreamResource, JsErrorBox> {
  let (read_tx, read_rx) = mpsc::unbounded_channel::<ReadEvent>();
  let handle_data = Box::new(UvTcpHandleData {
    read_tx: read_tx.clone(),
    read_buf: None,
  });

  // SAFETY: tcp handle is valid and data pointer is owned by libuv close callback.
  unsafe {
    deno_libuv::sys::uv_handle_set_data(
      tcp.cast(),
      Box::into_raw(handle_data).cast(),
    );
  }

  // SAFETY: callbacks are static and tcp handle is valid.
  let read_start_rc = unsafe {
    deno_libuv::sys::uv_read_start(
      tcp.cast(),
      Some(uv_tcp_alloc_cb),
      Some(uv_tcp_read_cb),
    )
  };

  if read_start_rc < 0 {
    close_uv_tcp_handle(&libuv_loop, tcp);
    return Err(uv_status_to_js_error(
      read_start_rc,
      "failed to start libuv tcp read",
    ));
  }

  Ok(UvTcpStreamResource::new(libuv_loop, tcp, read_tx, read_rx))
}

async fn uv_tcp_connect(
  libuv_loop: &LibUvLoop,
  addr: SocketAddr,
) -> Result<*mut deno_libuv::sys::uv_tcp_t, JsErrorBox> {
  // SAFETY: zeroed uv_tcp_t is acceptable before uv_tcp_init.
  let tcp = Box::into_raw(Box::new(unsafe {
    mem::zeroed::<deno_libuv::sys::uv_tcp_t>()
  }));

  // SAFETY: pointers are valid.
  let rc = unsafe {
    deno_libuv::sys::uv_tcp_init(libuv_loop.libuv_loop.as_mut_ptr(), tcp)
  };
  if rc < 0 {
    // SAFETY: uv_tcp_init failed, so no libuv state was attached.
    unsafe {
      drop(Box::from_raw(tcp));
    }
    return Err(uv_status_to_js_error(
      rc,
      "failed to initialize libuv tcp handle",
    ));
  }

  // SAFETY: zeroed uv_connect_t is acceptable before uv_tcp_connect.
  let connect_req = Box::into_raw(Box::new(unsafe {
    mem::zeroed::<deno_libuv::sys::uv_connect_t>()
  }));

  let (tx, rx) = oneshot::channel::<i32>();
  let connect_data = Box::new(ConnectReqData { tx: Some(tx) });

  // SAFETY: req pointer is valid.
  unsafe {
    deno_libuv::sys::uv_req_set_data(
      connect_req.cast::<deno_libuv::sys::uv_req_t>(),
      Box::into_raw(connect_data).cast::<c_void>(),
    );
  }

  // SAFETY: all pointers are valid and callbacks are static.
  let connect_rc = unsafe {
    match addr {
      SocketAddr::V4(addr_v4) => {
        let ip = CString::new(addr_v4.ip().to_string()).unwrap();
        let mut sock_addr = mem::zeroed::<deno_libuv::sys::sockaddr_in>();
        let rc = deno_libuv::sys::uv_ip4_addr(
          ip.as_ptr(),
          addr_v4.port() as i32,
          &mut sock_addr,
        );
        if rc < 0 {
          rc
        } else {
          deno_libuv::sys::uv_tcp_connect(
            connect_req,
            tcp,
            (&sock_addr as *const deno_libuv::sys::sockaddr_in).cast(),
            Some(uv_tcp_connect_cb),
          )
        }
      }
      SocketAddr::V6(addr_v6) => {
        let ip = CString::new(addr_v6.ip().to_string()).unwrap();
        let mut sock_addr = mem::zeroed::<deno_libuv::sys::sockaddr_in6>();
        let rc = deno_libuv::sys::uv_ip6_addr(
          ip.as_ptr(),
          addr_v6.port() as i32,
          &mut sock_addr,
        );
        if rc < 0 {
          rc
        } else {
          deno_libuv::sys::uv_tcp_connect(
            connect_req,
            tcp,
            (&sock_addr as *const deno_libuv::sys::sockaddr_in6).cast(),
            Some(uv_tcp_connect_cb),
          )
        }
      }
    }
  };

  if connect_rc < 0 {
    // SAFETY: req data still belongs to us on immediate error.
    unsafe {
      let data_ptr = deno_libuv::sys::uv_req_get_data(
        connect_req.cast::<deno_libuv::sys::uv_req_t>(),
      );
      if !data_ptr.is_null() {
        drop(Box::from_raw(data_ptr as *mut ConnectReqData));
      }
      drop(Box::from_raw(connect_req));
    }

    close_uv_tcp_handle(libuv_loop, tcp);
    return Err(uv_status_to_js_error(
      connect_rc,
      "libuv tcp connect failed",
    ));
  }

  libuv_loop.ensure_driver();

  let status = rx
    .await
    .map_err(|_| JsErrorBox::generic("libuv tcp connect was canceled"))?;

  if status < 0 {
    close_uv_tcp_handle(libuv_loop, tcp);
    return Err(uv_status_to_js_error(status, "libuv tcp connect failed"));
  }

  Ok(tcp)
}

struct UvTcpStreamResource {
  libuv_loop: LibUvLoop,
  tcp: *mut deno_libuv::sys::uv_tcp_t,
  read_tx: mpsc::UnboundedSender<ReadEvent>,
  read_rx: AsyncRefCell<mpsc::UnboundedReceiver<ReadEvent>>,
  read_buffer: RefCell<VecDeque<Vec<u8>>>,
}

impl UvTcpStreamResource {
  fn new(
    libuv_loop: LibUvLoop,
    tcp: *mut deno_libuv::sys::uv_tcp_t,
    read_tx: mpsc::UnboundedSender<ReadEvent>,
    read_rx: mpsc::UnboundedReceiver<ReadEvent>,
  ) -> Self {
    Self {
      libuv_loop,
      tcp,
      read_tx,
      read_rx: AsyncRefCell::new(read_rx),
      read_buffer: RefCell::new(VecDeque::new()),
    }
  }

  fn set_refed(&self, is_refed: bool) {
    // SAFETY: tcp handle is valid while resource exists.
    unsafe {
      if deno_libuv::sys::uv_is_closing(self.tcp.cast()) != 0 {
        return;
      }
      if is_refed {
        deno_libuv::sys::uv_ref(self.tcp.cast());
      } else {
        deno_libuv::sys::uv_unref(self.tcp.cast());
      }
    }
  }

  fn close_handle(&self) {
    // Wake any pending reads.
    let _ = self.read_tx.send(ReadEvent::Eof);

    // SAFETY: tcp handle is valid while resource exists.
    unsafe {
      if deno_libuv::sys::uv_is_closing(self.tcp.cast()) == 0 {
        let _ = deno_libuv::sys::uv_read_stop(self.tcp.cast());
        deno_libuv::sys::uv_close(self.tcp.cast(), Some(uv_tcp_close_cb));
      }
    }

    self.libuv_loop.ensure_driver();
  }

  async fn read(
    self: Rc<Self>,
    data: &mut [u8],
  ) -> Result<usize, std::io::Error> {
    if data.is_empty() {
      return Ok(0);
    }

    if let Some(mut buffered) = self.read_buffer.borrow_mut().pop_front() {
      let nread = buffered.len().min(data.len());
      data[..nread].copy_from_slice(&buffered[..nread]);
      if nread < buffered.len() {
        buffered.drain(..nread);
        self.read_buffer.borrow_mut().push_front(buffered);
      }
      return Ok(nread);
    }

    self.libuv_loop.ensure_driver();

    let event = {
      let mut receiver = RcRef::map(&self, |r| &r.read_rx).borrow_mut().await;
      receiver.recv().await
    };

    match event {
      Some(ReadEvent::Data(mut bytes)) => {
        let nread = bytes.len().min(data.len());
        data[..nread].copy_from_slice(&bytes[..nread]);
        if nread < bytes.len() {
          bytes.drain(..nread);
          self.read_buffer.borrow_mut().push_front(bytes);
        }
        Ok(nread)
      }
      Some(ReadEvent::Eof) | None => Ok(0),
      Some(ReadEvent::Error(status)) => {
        Err(uv_status_to_io_error(status, "libuv tcp read failed"))
      }
    }
  }

  async fn write(self: Rc<Self>, data: &[u8]) -> Result<usize, std::io::Error> {
    if data.is_empty() {
      return Ok(0);
    }

    // SAFETY: tcp handle is valid while resource exists.
    let is_closing =
      unsafe { deno_libuv::sys::uv_is_closing(self.tcp.cast()) != 0 };
    if is_closing {
      return Err(Error::new(
        ErrorKind::BrokenPipe,
        "libuv tcp handle is closing",
      ));
    }

    // SAFETY: zeroed uv_write_t is acceptable before uv_write.
    let write_req = Box::into_raw(Box::new(unsafe {
      mem::zeroed::<deno_libuv::sys::uv_write_t>()
    }));
    let (tx, rx) = oneshot::channel::<i32>();

    let mut write_data = Box::new(WriteReqData {
      tx: Some(tx),
      payload: data.to_vec().into_boxed_slice(),
    });

    // SAFETY: `payload` lives until callback frees WriteReqData.
    let uv_buf = unsafe {
      deno_libuv::sys::uv_buf_init(
        write_data.payload.as_mut_ptr().cast(),
        write_data.payload.len() as u32,
      )
    };

    // SAFETY: req and data pointers are valid.
    unsafe {
      deno_libuv::sys::uv_req_set_data(
        write_req.cast::<deno_libuv::sys::uv_req_t>(),
        Box::into_raw(write_data).cast::<c_void>(),
      );
    }

    // SAFETY: pointers are valid and callback is static.
    let rc = unsafe {
      deno_libuv::sys::uv_write(
        write_req,
        self.tcp.cast(),
        &uv_buf,
        1,
        Some(uv_tcp_write_cb),
      )
    };

    if rc < 0 {
      // SAFETY: on immediate error callback won't run; free allocations here.
      unsafe {
        let data_ptr = deno_libuv::sys::uv_req_get_data(
          write_req.cast::<deno_libuv::sys::uv_req_t>(),
        );
        if !data_ptr.is_null() {
          drop(Box::from_raw(data_ptr as *mut WriteReqData));
        }
        drop(Box::from_raw(write_req));
      }
      return Err(uv_status_to_io_error(rc, "libuv tcp write failed"));
    }

    self.libuv_loop.ensure_driver();

    let status = rx
      .await
      .map_err(|_| Error::other("libuv tcp write callback was canceled"))?;

    if status < 0 {
      return Err(uv_status_to_io_error(status, "libuv tcp write failed"));
    }

    Ok(data.len())
  }
}

impl Resource for UvTcpStreamResource {
  fn name(&self) -> Cow<'_, str> {
    "uvTcpStream".into()
  }

  fn close(self: Rc<Self>) {
    self.close_handle();
  }
}

struct UvTcpListenerResource {
  libuv_loop: LibUvLoop,
  listener: *mut deno_libuv::sys::uv_tcp_t,
  accept_tx: mpsc::UnboundedSender<AcceptEvent>,
  accept_rx: AsyncRefCell<mpsc::UnboundedReceiver<AcceptEvent>>,
}

impl UvTcpListenerResource {
  fn new(
    libuv_loop: LibUvLoop,
    listener: *mut deno_libuv::sys::uv_tcp_t,
    accept_tx: mpsc::UnboundedSender<AcceptEvent>,
    accept_rx: mpsc::UnboundedReceiver<AcceptEvent>,
  ) -> Self {
    Self {
      libuv_loop,
      listener,
      accept_tx,
      accept_rx: AsyncRefCell::new(accept_rx),
    }
  }

  fn set_refed(&self, is_refed: bool) {
    // SAFETY: listener handle is valid while resource exists.
    unsafe {
      if deno_libuv::sys::uv_is_closing(self.listener.cast()) != 0 {
        return;
      }
      if is_refed {
        deno_libuv::sys::uv_ref(self.listener.cast());
      } else {
        deno_libuv::sys::uv_unref(self.listener.cast());
      }
    }
  }

  fn close_handle(&self) {
    let _ = self.accept_tx.send(AcceptEvent::Closed);
    close_uv_tcp_listener_handle(&self.libuv_loop, self.listener);
  }

  async fn accept(
    self: &Rc<Self>,
  ) -> Result<*mut deno_libuv::sys::uv_tcp_t, std::io::Error> {
    self.libuv_loop.ensure_driver();

    let event = {
      let mut receiver = RcRef::map(self, |r| &r.accept_rx).borrow_mut().await;
      receiver.recv().await
    };

    match event {
      Some(AcceptEvent::Connection(client)) => Ok(client),
      Some(AcceptEvent::Error(status)) => {
        Err(uv_status_to_io_error(status, "libuv tcp accept failed"))
      }
      Some(AcceptEvent::Closed) | None => Err(Error::new(
        ErrorKind::BrokenPipe,
        "libuv tcp listener closed",
      )),
    }
  }
}

impl Resource for UvTcpListenerResource {
  fn name(&self) -> Cow<'_, str> {
    "uvTcpListener".into()
  }

  fn close(self: Rc<Self>) {
    self.close_handle();
  }
}

#[op2]
pub async fn op_uv_net_connect_tcp(
  op_state: Rc<RefCell<OpState>>,
  #[string] hostname: String,
  port: u16,
) -> Result<(ResourceId, IpAddr, IpAddr), JsErrorBox> {
  {
    let mut state = op_state.borrow_mut();
    state
      .borrow_mut::<PermissionsContainer>()
      .check_net(&(&hostname, Some(port)), "net.connect()")
      .map_err(JsErrorBox::from_err)?;
  }

  let addr = tokio::net::lookup_host((hostname.as_str(), port))
    .await
    .map_err(JsErrorBox::from_err)?
    .next()
    .ok_or_else(|| JsErrorBox::generic("No resolved address found"))?;

  let libuv_loop = LibUvLoop::get_or_create(op_state.clone());
  let tcp = uv_tcp_connect(&libuv_loop, addr).await?;

  let local_addr = get_tcp_address(tcp, false)?;
  let remote_addr = get_tcp_address(tcp, true)?;

  let resource = setup_uv_tcp_stream_resource(libuv_loop, tcp)?;

  let rid = op_state.borrow_mut().resource_table.add(resource);

  Ok((rid, local_addr, remote_addr))
}

#[op2]
pub fn op_uv_net_listen_tcp(
  op_state: &mut OpState,
  #[string] hostname: String,
  port: u16,
  backlog: i32,
) -> Result<(ResourceId, IpAddr), JsErrorBox> {
  op_state
    .borrow_mut::<PermissionsContainer>()
    .check_net(&(&hostname, Some(port)), "net.Server.listen()")
    .map_err(JsErrorBox::from_err)?;

  let addr = (hostname.as_str(), port)
    .to_socket_addrs()
    .map_err(JsErrorBox::from_err)?
    .next()
    .ok_or_else(|| JsErrorBox::generic("No resolved address found"))?;

  let libuv_loop = LibUvLoop::get_or_create_mut(op_state);

  let listener = Box::into_raw(Box::new(unsafe {
    mem::zeroed::<deno_libuv::sys::uv_tcp_t>()
  }));
  let init_rc = unsafe {
    deno_libuv::sys::uv_tcp_init(libuv_loop.libuv_loop.as_mut_ptr(), listener)
  };
  if init_rc < 0 {
    unsafe {
      drop(Box::from_raw(listener));
    }
    return Err(uv_status_to_js_error(
      init_rc,
      "failed to initialize libuv tcp listener",
    ));
  }

  let bind_rc = unsafe {
    match addr {
      SocketAddr::V4(addr_v4) => {
        let ip = CString::new(addr_v4.ip().to_string()).unwrap();
        let mut sock_addr = mem::zeroed::<deno_libuv::sys::sockaddr_in>();
        let rc = deno_libuv::sys::uv_ip4_addr(
          ip.as_ptr(),
          addr_v4.port() as i32,
          &mut sock_addr,
        );
        if rc < 0 {
          rc
        } else {
          deno_libuv::sys::uv_tcp_bind(
            listener,
            (&sock_addr as *const deno_libuv::sys::sockaddr_in).cast(),
            0,
          )
        }
      }
      SocketAddr::V6(addr_v6) => {
        let ip = CString::new(addr_v6.ip().to_string()).unwrap();
        let mut sock_addr = mem::zeroed::<deno_libuv::sys::sockaddr_in6>();
        let rc = deno_libuv::sys::uv_ip6_addr(
          ip.as_ptr(),
          addr_v6.port() as i32,
          &mut sock_addr,
        );
        if rc < 0 {
          rc
        } else {
          deno_libuv::sys::uv_tcp_bind(
            listener,
            (&sock_addr as *const deno_libuv::sys::sockaddr_in6).cast(),
            0,
          )
        }
      }
    }
  };

  if bind_rc < 0 {
    close_uv_tcp_listener_handle(&libuv_loop, listener);
    return Err(uv_status_to_js_error(
      bind_rc,
      "failed to bind libuv tcp listener",
    ));
  }

  let (accept_tx, accept_rx) = mpsc::unbounded_channel::<AcceptEvent>();
  let handle_data = Box::new(UvTcpListenerHandleData {
    accept_tx: accept_tx.clone(),
  });
  unsafe {
    deno_libuv::sys::uv_handle_set_data(
      listener.cast(),
      Box::into_raw(handle_data).cast(),
    );
  }

  let listen_rc = unsafe {
    deno_libuv::sys::uv_listen(
      listener.cast(),
      backlog.max(1),
      Some(uv_tcp_server_connection_cb),
    )
  };
  if listen_rc < 0 {
    close_uv_tcp_listener_handle(&libuv_loop, listener);
    return Err(uv_status_to_js_error(
      listen_rc,
      "failed to listen on libuv tcp listener",
    ));
  }

  libuv_loop.ensure_driver();

  let local_addr = get_tcp_address(listener, false)?;
  let resource =
    UvTcpListenerResource::new(libuv_loop, listener, accept_tx, accept_rx);
  let rid = op_state.resource_table.add(resource);
  Ok((rid, local_addr))
}

#[op2]
pub async fn op_uv_net_accept_tcp(
  op_state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<(ResourceId, IpAddr, IpAddr), JsErrorBox> {
  let listener = op_state
    .borrow()
    .resource_table
    .get::<UvTcpListenerResource>(rid)
    .map_err(JsErrorBox::from_err)?;

  let tcp = listener.accept().await.map_err(JsErrorBox::from_err)?;
  let local_addr = get_tcp_address(tcp, false)?;
  let remote_addr = get_tcp_address(tcp, true)?;
  let stream_resource =
    setup_uv_tcp_stream_resource(listener.libuv_loop.clone(), tcp)?;
  let stream_rid = op_state.borrow_mut().resource_table.add(stream_resource);
  Ok((stream_rid, local_addr, remote_addr))
}

#[op2]
pub async fn op_uv_net_read(
  op_state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  #[buffer] mut data: JsBuffer,
) -> Result<u32, JsErrorBox> {
  let resource = op_state
    .borrow()
    .resource_table
    .get::<UvTcpStreamResource>(rid)
    .map_err(JsErrorBox::from_err)?;

  let nread = resource
    .read(&mut data)
    .await
    .map_err(JsErrorBox::from_err)?;
  Ok(u32::try_from(nread).unwrap_or(u32::MAX))
}

#[op2]
pub async fn op_uv_net_write(
  op_state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  #[buffer] data: JsBuffer,
) -> Result<u32, JsErrorBox> {
  let resource = op_state
    .borrow()
    .resource_table
    .get::<UvTcpStreamResource>(rid)
    .map_err(JsErrorBox::from_err)?;

  let nwritten = resource.write(&data).await.map_err(JsErrorBox::from_err)?;
  Ok(u32::try_from(nwritten).unwrap_or(u32::MAX))
}

#[op2(fast)]
pub fn op_uv_net_ref(
  op_state: &mut OpState,
  #[smi] rid: ResourceId,
) -> Result<(), JsErrorBox> {
  let resource = op_state
    .resource_table
    .get::<UvTcpStreamResource>(rid)
    .map_err(JsErrorBox::from_err)?;
  resource.set_refed(true);
  Ok(())
}

#[op2(fast)]
pub fn op_uv_net_unref(
  op_state: &mut OpState,
  #[smi] rid: ResourceId,
) -> Result<(), JsErrorBox> {
  let resource = op_state
    .resource_table
    .get::<UvTcpStreamResource>(rid)
    .map_err(JsErrorBox::from_err)?;
  resource.set_refed(false);
  Ok(())
}

#[op2(fast)]
pub fn op_uv_net_listener_ref(
  op_state: &mut OpState,
  #[smi] rid: ResourceId,
) -> Result<(), JsErrorBox> {
  let resource = op_state
    .resource_table
    .get::<UvTcpListenerResource>(rid)
    .map_err(JsErrorBox::from_err)?;
  resource.set_refed(true);
  Ok(())
}

#[op2(fast)]
pub fn op_uv_net_listener_unref(
  op_state: &mut OpState,
  #[smi] rid: ResourceId,
) -> Result<(), JsErrorBox> {
  let resource = op_state
    .resource_table
    .get::<UvTcpListenerResource>(rid)
    .map_err(JsErrorBox::from_err)?;
  resource.set_refed(false);
  Ok(())
}
