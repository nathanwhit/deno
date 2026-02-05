use std::cell::RefCell;
use std::rc::Rc;

use deno_core::OpState;
use deno_core::op2;
use deno_error::JsErrorBox;

#[derive(Clone)]
pub struct LibUvLoop {
  libuv_loop: Rc<deno_libuv::UvLoop>,
}

impl LibUvLoop {
  pub fn new() -> Self {
    let libuv_loop =
      deno_libuv::UvLoop::new().expect("Failed to create libuv loop");
    Self { libuv_loop }
  }
}

impl Drop for LibUvLoop {
  fn drop(&mut self) {
    self.libuv_loop.stop();
  }
}

fn get_or_create_libuv_loop(op_state: Rc<RefCell<OpState>>) -> LibUvLoop {}

#[op2]
pub async fn op_uv_net_connect_tcp(
  op_state: Rc<RefCell<OpState>>,
  #[string] hostname: String,
  port: u16,
) -> Result<(), JsErrorBox> {
}
