// Copyright 2018-2026 the Deno authors. MIT license.

use std::ptr::NonNull;
use std::rc::Rc;

use crate::error::check;
use crate::error::UvError;
use crate::sys::uv_loop_alive;
use crate::sys::uv_loop_close;
use crate::sys::uv_loop_init;
use crate::sys::uv_loop_t;
use crate::sys::uv_run;
use crate::sys::uv_run_mode_UV_RUN_DEFAULT;
use crate::sys::uv_run_mode_UV_RUN_NOWAIT;
use crate::sys::uv_run_mode_UV_RUN_ONCE;
use crate::sys::uv_stop;

/// Mode for `UvLoop::run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
  /// Run the event loop until there are no more active handles/requests.
  Default,
  /// Poll for I/O once. May block if there are no pending callbacks.
  Once,
  /// Poll for I/O once but don't block if there are no pending callbacks.
  NoWait,
}

impl RunMode {
  fn as_raw(self) -> u32 {
    match self {
      RunMode::Default => uv_run_mode_UV_RUN_DEFAULT,
      RunMode::Once => uv_run_mode_UV_RUN_ONCE,
      RunMode::NoWait => uv_run_mode_UV_RUN_NOWAIT,
    }
  }
}

/// A libuv event loop.
///
/// Created via `UvLoop::new()` which returns an `Rc<UvLoop>`. Handles hold
/// an `Rc<UvLoop>` to keep the loop alive as long as handles exist.
pub struct UvLoop {
  ptr: NonNull<uv_loop_t>,
}

impl UvLoop {
  /// Allocate and initialize a new event loop.
  pub fn new() -> Result<Rc<UvLoop>, UvError> {
    // Allocate the loop on the heap for a stable address.
    let boxed = Box::new(unsafe { std::mem::zeroed::<uv_loop_t>() });
    let raw = Box::into_raw(boxed);
    let rc = unsafe { uv_loop_init(raw) };
    check(rc)?;
    Ok(Rc::new(UvLoop {
      ptr: unsafe { NonNull::new_unchecked(raw) },
    }))
  }

  /// Wrap an already-initialized `uv_loop_t` pointer.
  ///
  /// # Safety
  ///
  /// - `ptr` must point to a live, initialized `uv_loop_t` that was
  ///   heap-allocated (e.g. via `Box::into_raw`).
  /// - The caller transfers ownership of the allocation — the returned
  ///   `UvLoop` will call `uv_loop_close` and free the memory on drop.
  /// - No other code may close or free this loop.
  pub unsafe fn from_raw(ptr: *mut uv_loop_t) -> Rc<UvLoop> {
    Rc::new(UvLoop {
      ptr: unsafe { NonNull::new_unchecked(ptr) },
    })
  }

  /// Returns the raw pointer to the underlying `uv_loop_t`.
  ///
  /// The pointer is valid for the lifetime of this `UvLoop`. Use it to
  /// call [`sys`](crate::sys) functions directly.
  pub fn as_mut_ptr(&self) -> *mut uv_loop_t {
    self.ptr.as_ptr()
  }

  /// Run the event loop in the given mode.
  ///
  /// Returns `true` if there are still active handles or requests
  /// (only meaningful for `RunMode::Once` and `RunMode::NoWait`).
  pub fn run(&self, mode: RunMode) -> bool {
    let rc = unsafe { uv_run(self.ptr.as_ptr(), mode.as_raw()) };
    rc != 0
  }

  /// Stop the event loop. Causes `run` to return as soon as possible.
  pub fn stop(&self) {
    unsafe { uv_stop(self.ptr.as_ptr()) };
  }

  /// Returns `true` if the loop has active handles or requests.
  pub fn alive(&self) -> bool {
    unsafe { uv_loop_alive(self.ptr.as_ptr()) != 0 }
  }
}

impl Drop for UvLoop {
  fn drop(&mut self) {
    unsafe {
      let rc = uv_loop_close(self.ptr.as_ptr());
      debug_assert_eq!(rc, 0, "uv_loop_close failed (active handles remain?)");
      // Reclaim the Box allocation.
      drop(Box::from_raw(self.ptr.as_ptr()));
    }
  }
}
