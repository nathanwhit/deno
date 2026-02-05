use std::cell::RefCell;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::rc::Rc;

use deno_core::OpState;
use deno_core::op2;
use deno_error::JsErrorBox;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Newtype wrapper around a raw fd that does NOT close on drop.
/// Used to register libuv's backend fd with tokio's reactor without
/// transferring ownership.
struct BackendFd(RawFd);

impl AsRawFd for BackendFd {
  fn as_raw_fd(&self) -> RawFd {
    self.0
  }
}

// /// Drives a libuv event loop cooperatively inside a tokio current-thread
// /// runtime. The libuv backend fd (kqueue/epoll) is registered with tokio's
// /// reactor so both loops share the same thread without busy-waiting.
// pub struct TokioUvLoop {
//     // Order matters: async_fd must be dropped (deregistered from reactor)
//     // before uv_loop closes the backend fd.
//     async_fd: AsyncFd<BackendFd>,
//     uv_loop: Loop,
// }

// impl TokioUvLoop {
//     /// Create a new libuv loop integrated with the current tokio runtime.
//     /// Must be called from within a tokio context.
//     pub fn new() -> Result<Self> {
//         let uv_loop = Loop::new()?;
//         let fd = uv_loop.backend_fd();
//         // SAFETY: The fd is valid for the lifetime of uv_loop and we ensure
//         // async_fd is dropped before uv_loop via struct field order.
//         let async_fd = AsyncFd::with_interest(BackendFd(fd), Interest::READABLE)
//             .expect("failed to register libuv backend fd with tokio");
//         Ok(Self { async_fd, uv_loop })
//     }

//     /// Access the underlying libuv loop to create handles before driving.
//     pub fn uv_loop(&mut self) -> &mut Loop {
//         &mut self.uv_loop
//     }

//     /// Drive the libuv event loop to completion. This async function
//     /// cooperatively yields to tokio between iterations, allowing tokio
//     /// tasks to run on the same thread.
//     pub async fn drive(&mut self) {
//         loop {
//             // Non-blocking poll: process any ready libuv work.
//             let _ = self.uv_loop.run(RunMode::NoWait);

//             if !self.uv_loop.is_alive() {
//                 break;
//             }

//             let timeout = self.uv_loop.backend_timeout();

//             if timeout == 0 {
//                 // Pending work to do — yield so tokio tasks can also run,
//                 // then come back immediately.
//                 tokio::task::yield_now().await;
//             } else if timeout < 0 {
//                 // No timer deadlines — wait purely for I/O on the backend fd.
//                 if let Ok(mut guard) = self.async_fd.readable().await {
//                     guard.clear_ready();
//                 }
//             } else {
//                 // Wait for either I/O readiness or the next timer deadline.
//                 let sleep = tokio::time::sleep(
//                     std::time::Duration::from_millis(timeout as u64),
//                 );
//                 tokio::pin!(sleep);
//                 tokio::select! {
//                     guard = self.async_fd.readable() => {
//                         if let Ok(mut guard) = guard {
//                             guard.clear_ready();
//                         }
//                     }
//                     _ = &mut sleep => {}
//                 }
//             }
//         }
//     }
// }

#[derive(Clone)]
pub struct LibUvLoop {
  libuv_loop: Rc<deno_libuv::UvLoop>,
}

pub struct LibUvLoopDriver {
  async_fd: AsyncFd<BackendFd>,
  libuv_loop: Rc<deno_libuv::UvLoop>,
}

impl LibUvLoopDriver {
  pub fn new(libuv_loop: Rc<deno_libuv::UvLoop>) -> Self {
    let fd =
      BackendFd(unsafe { deno_libuv::sys::uv_backend_fd(libuv_loop.as_ptr()) });
    let async_fd = AsyncFd::with_interest(fd, Interest::READABLE).unwrap();
    Self {
      libuv_loop,
      async_fd,
    }
  }
  pub async fn drive(&mut self) {
    loop {
      let _ = self.libuv_loop.run(deno_libuv::RunMode::NoWait);
      if !self.libuv_loop.alive() {
        break;
      }
      if let Ok(mut guard) = self.async_fd.readable().await {
        guard.clear_ready();
      }
    }
  }
}

impl LibUvLoop {
  pub fn new() -> Self {
    let libuv_loop =
      deno_libuv::UvLoop::new().expect("Failed to create libuv loop");

    deno_core::unsync::spawn({
      let libuv_loop = libuv_loop.clone();
      async move {
        let mut driver = LibUvLoopDriver::new(libuv_loop);
        driver.drive().await;
      }
    });

    LibUvLoop { libuv_loop }
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
}

impl Drop for LibUvLoop {
  fn drop(&mut self) {
    self.libuv_loop.stop();
  }
}

#[op2]
pub async fn op_uv_net_connect_tcp(
  op_state: Rc<RefCell<OpState>>,
  #[string] hostname: String,
  port: u16,
) -> Result<(), JsErrorBox> {
  todo!()
}
