// Copyright 2018-2026 the Deno authors. MIT license.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use deno_core::CppgcBase;
use deno_core::CppgcInherits;
use deno_core::GarbageCollected;
use deno_core::OpState;
use deno_core::ResourceId;
use deno_core::op2;
use deno_core::v8;

use super::handle_wrap::AsyncWrap;
use super::handle_wrap::HandleWrap;

// `providerType.SHUTDOWNWRAP`
const PROVIDER_SHUTDOWNWRAP: i32 = 28;
// `providerType.WRITEWRAP`
const PROVIDER_WRITEWRAP: i32 = 41;

// Keep this aligned with Node's `kReadBytesOrError`, `kArrayBufferOffset`,
// `kBytesWritten`, `kLastWriteWasAsync`, `kNumStreamBaseStateFields`.
const K_BYTES_WRITTEN: usize = 2;
const K_LAST_WRITE_WAS_ASYNC: usize = 3;

#[derive(Default)]
pub struct StreamBaseState(pub [u32; 5]);

impl StreamBaseState {
  fn set_last_write_result(&mut self, bytes_written: u32, is_async: bool) {
    self.0[K_BYTES_WRITTEN] = bytes_written;
    self.0[K_LAST_WRITE_WAS_ASYNC] = is_async as u32;
  }
}

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(AsyncWrap)]
#[repr(C)]
pub struct WriteWrap {
  base: AsyncWrap,
}

// SAFETY: we're sure this can be GCed
unsafe impl GarbageCollected for WriteWrap {
  fn trace(&self, visitor: &mut deno_core::v8::cppgc::Visitor) {
    self.base.trace(visitor);
  }

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"WriteWrap"
  }
}

#[op2(base, inherit = AsyncWrap)]
impl WriteWrap {
  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_wrap.cc#L93-L100
  #[constructor]
  #[cppgc]
  fn new(state: &mut OpState) -> WriteWrap {
    WriteWrap {
      base: AsyncWrap::create(state, PROVIDER_WRITEWRAP),
    }
  }
}

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(AsyncWrap)]
#[repr(C)]
pub struct ShutdownWrap {
  base: AsyncWrap,
}

// SAFETY: we're sure this can be GCed
unsafe impl GarbageCollected for ShutdownWrap {
  fn trace(&self, visitor: &mut deno_core::v8::cppgc::Visitor) {
    self.base.trace(visitor);
  }

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"ShutdownWrap"
  }
}

#[op2(base, inherit = AsyncWrap)]
impl ShutdownWrap {
  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_wrap.cc#L73-L92
  #[constructor]
  #[cppgc]
  fn new(state: &mut OpState) -> ShutdownWrap {
    ShutdownWrap {
      base: AsyncWrap::create(state, PROVIDER_SHUTDOWNWRAP),
    }
  }
}

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(HandleWrap)]
#[repr(C)]
pub struct LibuvStreamWrap {
  base: HandleWrap,
  reading: Cell<bool>,
  write_queue_size: Cell<u32>,
  bytes_read: Cell<u64>,
  bytes_written: Cell<u64>,
}

// SAFETY: we're sure this can be GCed
unsafe impl GarbageCollected for LibuvStreamWrap {
  fn trace(&self, _visitor: &mut deno_core::v8::cppgc::Visitor) {}

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"LibuvStreamWrap"
  }
}

impl LibuvStreamWrap {
  pub(crate) fn create(handle_wrap: HandleWrap) -> Self {
    Self {
      base: handle_wrap,
      reading: Cell::new(false),
      write_queue_size: Cell::new(0),
      bytes_read: Cell::new(0),
      bytes_written: Cell::new(0),
    }
  }
}

static ON_CLOSE_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("_onClose");
static ON_COMPLETE_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("oncomplete");

#[op2(base, inherit = HandleWrap)]
impl LibuvStreamWrap {
  #[constructor]
  #[cppgc]
  fn new(
    #[smi] provider: i32,
    #[smi] handle: Option<ResourceId>,
    state: &mut OpState,
  ) -> LibuvStreamWrap {
    LibuvStreamWrap::create(HandleWrap::create(
      AsyncWrap::create(state, provider),
      handle,
    ))
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_wrap.cc#L201-L215
  #[fast]
  fn read_start(&self) -> i32 {
    self.reading.set(true);
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_wrap.cc#L217-L220
  #[fast]
  fn read_stop(&self) -> i32 {
    self.reading.set(false);
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_wrap.cc#L296-L308
  #[getter]
  fn write_queue_size(&self) -> u32 {
    self.write_queue_size.get()
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_wrap.cc#L311-L322
  #[fast]
  fn set_blocking(&self, _enabled: bool) -> i32 {
    if !self.base.is_alive() {
      return -22; // UV_EINVAL
    }
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_base.cc#L168-L173
  #[reentrant]
  fn shutdown(
    &self,
    scope: &mut v8::PinScope<'_, '_>,
    #[this] this: v8::Global<v8::Object>,
    #[scoped] req: v8::Global<v8::Object>,
  ) -> i32 {
    let status = call_on_close(scope, this);
    call_oncomplete(scope, req, status);
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_base.cc#L160-L166
  #[fast]
  fn use_user_buffer(&self) -> i32 {
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_base.cc#L297-L335
  #[reentrant]
  fn write_buffer(
    &self,
    op_state: Rc<RefCell<OpState>>,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] req: v8::Global<v8::Object>,
    #[buffer] data: &[u8],
  ) -> i32 {
    let bytes_written = data.len().min(u32::MAX as usize) as u32;
    update_write_result(&op_state, bytes_written);
    self
      .bytes_written
      .set(self.bytes_written.get().saturating_add(data.len() as u64));
    call_oncomplete(scope, req, 0);
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_base.cc#L180-L295
  #[reentrant]
  fn writev(
    &self,
    op_state: Rc<RefCell<OpState>>,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] req: v8::Global<v8::Object>,
  ) -> i32 {
    update_write_result(&op_state, 0);
    call_oncomplete(scope, req, 0);
    0
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_base.cc#L141-L149
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/stream_base.cc#L337-L445
  #[reentrant]
  fn write_ascii_string(
    &self,
    op_state: Rc<RefCell<OpState>>,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] data: &str,
  ) -> i32 {
    self.complete_string_write(&op_state, scope, req, data)
  }

  #[reentrant]
  fn write_utf8_string(
    &self,
    op_state: Rc<RefCell<OpState>>,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] data: &str,
  ) -> i32 {
    self.complete_string_write(&op_state, scope, req, data)
  }

  #[reentrant]
  fn write_ucs2_string(
    &self,
    op_state: Rc<RefCell<OpState>>,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] data: &str,
  ) -> i32 {
    self.complete_string_write(&op_state, scope, req, data)
  }

  #[reentrant]
  fn write_latin1_string(
    &self,
    op_state: Rc<RefCell<OpState>>,
    scope: &mut v8::PinScope<'_, '_>,
    #[scoped] req: v8::Global<v8::Object>,
    #[string] data: &str,
  ) -> i32 {
    self.complete_string_write(&op_state, scope, req, data)
  }

  #[getter]
  #[number]
  fn bytes_read(&self) -> u64 {
    self.bytes_read.get()
  }

  #[getter]
  #[number]
  fn bytes_written(&self) -> u64 {
    self.bytes_written.get()
  }
}

impl LibuvStreamWrap {
  fn complete_string_write(
    &self,
    op_state: &Rc<RefCell<OpState>>,
    scope: &mut v8::PinScope<'_, '_>,
    req: v8::Global<v8::Object>,
    data: &str,
  ) -> i32 {
    let bytes_written = data.len().min(u32::MAX as usize) as u32;
    update_write_result(op_state, bytes_written);
    self
      .bytes_written
      .set(self.bytes_written.get().saturating_add(data.len() as u64));
    call_oncomplete(scope, req, 0);
    0
  }
}

fn update_write_result(op_state: &Rc<RefCell<OpState>>, bytes_written: u32) {
  if op_state.borrow().try_borrow::<StreamBaseState>().is_none() {
    op_state.borrow_mut().put(StreamBaseState::default());
  }

  op_state
    .borrow_mut()
    .borrow_mut::<StreamBaseState>()
    .set_last_write_result(bytes_written, true);
}

fn call_on_close(
  scope: &mut v8::PinScope<'_, '_>,
  this: v8::Global<v8::Object>,
) -> i32 {
  let this = v8::Local::new(scope, this);
  let on_close_str = ON_CLOSE_STR.v8_string(scope).unwrap();
  let on_close = this.get(scope, on_close_str.into());
  let Some(on_close) = on_close else {
    return 0;
  };
  let Ok(on_close) = v8::Local::<v8::Function>::try_from(on_close) else {
    return 0;
  };

  let Some(value) = on_close.call(scope, this.into(), &[]) else {
    return 0;
  };

  value.int32_value(scope).unwrap_or(0)
}

fn call_oncomplete(
  scope: &mut v8::PinScope<'_, '_>,
  req: v8::Global<v8::Object>,
  status: i32,
) {
  let req = v8::Local::new(scope, req);
  let on_complete_str = ON_COMPLETE_STR.v8_string(scope).unwrap();
  let on_complete = req.get(scope, on_complete_str.into());
  let Some(on_complete) = on_complete else {
    return;
  };
  let Ok(on_complete) = v8::Local::<v8::Function>::try_from(on_complete) else {
    return;
  };

  let status = v8::Integer::new(scope, status);
  on_complete.call(scope, req.into(), &[status.into()]);
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
        super::ShutdownWrap,
        super::WriteWrap,
        super::LibuvStreamWrap,
      ],
      state = |state| {
        state.put::<super::super::handle_wrap::AsyncId>(
          super::super::handle_wrap::AsyncId::default(),
        );
        state.put::<super::StreamBaseState>(super::StreamBaseState::default());
      }
    );

    let mut runtime = JsRuntime::new(RuntimeOptions {
      extensions: vec![test_ext::init()],
      ..Default::default()
    });

    poll_fn(move |cx| {
      runtime
        .execute_script("file://stream_wrap_test.js", source_code)
        .unwrap();

      let result = runtime.poll_event_loop(cx, Default::default());
      assert!(matches!(result, Poll::Ready(Ok(()))));
      Poll::Ready(())
    })
    .await;
  }

  #[tokio::test(flavor = "current_thread")]
  async fn test_libuv_stream_wrap_write_and_shutdown() {
    js_test(
      r#"
        const { LibuvStreamWrap, WriteWrap, ShutdownWrap } = Deno.core.ops;

        let onCloseCalled = false;
        class TestStreamWrap extends LibuvStreamWrap {
          constructor() {
            super(0, null);
          }

          _onClose() {
            onCloseCalled = true;
            return 0;
          }
        }

        const stream = new TestStreamWrap();
        const writeReq = new WriteWrap();
        let writeStatus = undefined;
        writeReq.oncomplete = (status) => {
          writeStatus = status;
        };

        stream.writeBuffer(writeReq, new Uint8Array([1, 2, 3]));
        if (writeStatus !== 0) {
          throw new Error("WriteWrap.oncomplete was not called with status 0");
        }

        const shutdownReq = new ShutdownWrap();
        let shutdownStatus = undefined;
        shutdownReq.oncomplete = (status) => {
          shutdownStatus = status;
        };

        stream.shutdown(shutdownReq);

        if (!onCloseCalled) {
          throw new Error("LibuvStreamWrap._onClose was not called");
        }
        if (shutdownStatus !== 0) {
          throw new Error("ShutdownWrap.oncomplete was not called with status 0");
        }
      "#,
    )
    .await;
  }
}
