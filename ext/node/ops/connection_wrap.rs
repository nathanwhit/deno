// Copyright 2018-2026 the Deno authors. MIT license.

use deno_core::CppgcBase;
use deno_core::CppgcInherits;
use deno_core::GarbageCollected;
use deno_core::OpState;
use deno_core::op2;
use deno_core::v8;

use super::stream_wrap::LibuvStreamWrap;

#[derive(CppgcBase, CppgcInherits)]
#[cppgc_inherits_from(LibuvStreamWrap)]
#[repr(C)]
pub struct ConnectionWrap {
  pub(crate) stream_wrap: LibuvStreamWrap,
}

// SAFETY: instances are prevented from preventing garbage collection
// by ensuring the stored Global is cleared on close.
unsafe impl GarbageCollected for ConnectionWrap {
  fn trace(&self, visitor: &mut v8::cppgc::Visitor) {
    self.stream_wrap.trace(visitor);
  }

  fn get_name(&self) -> &'static std::ffi::CStr {
    c"ConnectionWrap"
  }
}

impl ConnectionWrap {
  pub(crate) fn create(
    this: v8::Global<v8::Object>,
    state: &mut OpState,
    provider: i32,
  ) -> ConnectionWrap {
    ConnectionWrap {
      stream_wrap: LibuvStreamWrap::create(this, state, provider),
    }
  }
}

#[op2(base, inherit = LibuvStreamWrap)]
impl ConnectionWrap {
  #[constructor]
  #[cppgc]
  fn new(
    #[this] this: v8::Global<v8::Object>,
    state: &mut OpState,
    #[smi] provider: i32,
  ) -> ConnectionWrap {
    ConnectionWrap::create(this, state, provider)
  }
}
