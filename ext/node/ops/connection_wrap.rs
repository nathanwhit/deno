// Copyright 2018-2026 the Deno authors. MIT license.

use std::cell::Cell;
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
use super::stream_wrap::LibuvStreamWrap;

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(LibuvStreamWrap)]
#[repr(C)]
pub struct ConnectionWrap {
  base: LibuvStreamWrap,
}

// SAFETY: we're sure this can be GCed
unsafe impl GarbageCollected for ConnectionWrap {
  fn trace(&self, _visitor: &mut deno_core::v8::cppgc::Visitor) {}

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"ConnectionWrap"
  }
}

impl ConnectionWrap {
  pub(crate) fn create(libuv_stream_wrap: LibuvStreamWrap) -> Self {
    Self {
      base: libuv_stream_wrap,
    }
  }

  pub(crate) fn stream_reading_cell(&self) -> Rc<Cell<bool>> {
    self.base.reading_cell()
  }

  pub(crate) fn stream_write_queue_size_cell(&self) -> Rc<Cell<u32>> {
    self.base.write_queue_size_cell()
  }

  pub(crate) fn stream_bytes_read_cell(&self) -> Rc<Cell<u64>> {
    self.base.bytes_read_cell()
  }

  pub(crate) fn stream_bytes_written_cell(&self) -> Rc<Cell<u64>> {
    self.base.bytes_written_cell()
  }
}

static ON_CONNECTION_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("onconnection");
static ON_COMPLETE_STR: deno_core::FastStaticString =
  deno_core::ascii_str!("oncomplete");

#[op2(base, inherit = LibuvStreamWrap)]
impl ConnectionWrap {
  #[constructor]
  #[cppgc]
  fn new(
    #[smi] provider: i32,
    #[smi] handle: Option<ResourceId>,
    state: &mut OpState,
  ) -> ConnectionWrap {
    ConnectionWrap::create(LibuvStreamWrap::create(HandleWrap::create(
      AsyncWrap::create(state, provider),
      handle,
    )))
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/connection_wrap.cc#L33-L76
  #[reentrant]
  fn on_connection(
    &self,
    scope: &mut v8::PinScope<'_, '_>,
    #[this] this: v8::Global<v8::Object>,
    #[smi] status: i32,
    #[scoped] client_handle: Option<v8::Global<v8::Object>>,
  ) {
    let this = v8::Local::new(scope, this);
    let on_connection_str = ON_CONNECTION_STR.v8_string(scope).unwrap();
    let on_connection = this.get(scope, on_connection_str.into());
    let Some(on_connection) = on_connection else {
      return;
    };
    let Ok(on_connection) = v8::Local::<v8::Function>::try_from(on_connection)
    else {
      return;
    };

    let status = v8::Integer::new(scope, status);
    let client_handle = client_handle
      .map(|h| v8::Local::new(scope, h).into())
      .unwrap_or_else(|| v8::undefined(scope).into());
    on_connection.call(scope, this.into(), &[status.into(), client_handle]);
  }

  // Ported from Node.js
  //
  // https://github.com/nodejs/node/blob/18f695298ecf8f284d2ff9997b0f4f6f9664b2fa/src/connection_wrap.cc#L78-L116
  #[reentrant]
  fn after_connect(
    &self,
    scope: &mut v8::PinScope<'_, '_>,
    #[this] this: v8::Global<v8::Object>,
    #[scoped] req: v8::Global<v8::Object>,
    #[smi] status: i32,
  ) {
    let this = v8::Local::new(scope, this);
    let req = v8::Local::new(scope, req);

    let on_complete_str = ON_COMPLETE_STR.v8_string(scope).unwrap();
    let on_complete = req.get(scope, on_complete_str.into());
    let Some(on_complete) = on_complete else {
      return;
    };
    let Ok(on_complete) = v8::Local::<v8::Function>::try_from(on_complete)
    else {
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
        .execute_script("file://connection_wrap_test.js", source_code)
        .unwrap();

      let result = runtime.poll_event_loop(cx, Default::default());
      assert!(matches!(result, Poll::Ready(Ok(()))));
      Poll::Ready(())
    })
    .await;
  }

  #[tokio::test(flavor = "current_thread")]
  async fn test_connection_wrap_after_connect() {
    js_test(
      r#"
        const { ConnectionWrap } = Deno.core.ops;

        class TestConnectionWrap extends ConnectionWrap {
          constructor() {
            super(0, null);
          }
        }

        const handle = new TestConnectionWrap();
        const req = {};
        let called = false;
        req.oncomplete = (status, recvHandle, recvReq, readable, writable) => {
          called = true;
          if (status !== 0 || recvHandle !== handle || recvReq !== req) {
            throw new Error("afterConnect args are wrong");
          }
          if (!readable || !writable) {
            throw new Error("afterConnect should mark success as readable+writable");
          }
        };

        handle.afterConnect(req, 0);
        if (!called) {
          throw new Error("ConnectionWrap.afterConnect did not call req.oncomplete");
        }
      "#,
    )
    .await;
  }
}
