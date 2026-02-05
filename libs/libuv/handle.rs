// Copyright 2018-2026 the Deno authors. MIT license.

use std::ffi::c_void;
use std::rc::Rc;

use crate::error::UvError;
use crate::error::check;
use crate::loop_::UvLoop;
use crate::sys::uv_check_init;
use crate::sys::uv_check_start;
use crate::sys::uv_check_stop;
use crate::sys::uv_check_t;
use crate::sys::uv_close;
use crate::sys::uv_handle_get_data;
use crate::sys::uv_handle_set_data;
use crate::sys::uv_handle_t;
use crate::sys::uv_idle_init;
use crate::sys::uv_idle_start;
use crate::sys::uv_idle_stop;
use crate::sys::uv_idle_t;
use crate::sys::uv_prepare_init;
use crate::sys::uv_prepare_start;
use crate::sys::uv_prepare_stop;
use crate::sys::uv_prepare_t;
use crate::sys::uv_timer_again;
use crate::sys::uv_timer_get_repeat;
use crate::sys::uv_timer_init;
use crate::sys::uv_timer_set_repeat;
use crate::sys::uv_timer_start;
use crate::sys::uv_timer_stop;
use crate::sys::uv_timer_t;

// ---------------------------------------------------------------------------
// Callback data stored in each handle's `data` field
// ---------------------------------------------------------------------------

struct HandleData {
  callback: Option<Box<dyn FnMut()>>,
}

/// Install a fresh `HandleData` in the handle's `data` field.
///
/// SAFETY: `handle` must be a valid, initialized libuv handle.
unsafe fn init_handle_data(handle: *mut uv_handle_t) {
  let hd = Box::new(HandleData { callback: None });
  unsafe {
    uv_handle_set_data(handle, Box::into_raw(hd) as *mut c_void);
  }
}

// ---------------------------------------------------------------------------
// Close infrastructure
//
// `uv_close` is asynchronous – the handle memory must survive until the close
// callback fires.  We pack the cleanup info into a `CloseInfo`, store it in
// the handle's `data` field, and free everything in the close trampoline.
// ---------------------------------------------------------------------------

struct CloseInfo {
  handle_ptr: *mut c_void,
  dealloc_fn: unsafe fn(*mut c_void),
  // Prevent the loop from being freed before the close callback runs.
  _loop: Rc<UvLoop>,
}

unsafe extern "C" fn close_trampoline(handle: *mut uv_handle_t) {
  let data = unsafe { uv_handle_get_data(handle) };
  let info = unsafe { Box::from_raw(data as *mut CloseInfo) };
  unsafe { (info.dealloc_fn)(info.handle_ptr) };
  // `info` is dropped here, which drops the `Rc<UvLoop>`.
}

// Per-type dealloc functions (type-erased via fn pointer).
unsafe fn dealloc_idle(ptr: *mut c_void) {
  drop(unsafe { Box::from_raw(ptr as *mut uv_idle_t) });
}
unsafe fn dealloc_timer(ptr: *mut c_void) {
  drop(unsafe { Box::from_raw(ptr as *mut uv_timer_t) });
}
unsafe fn dealloc_prepare(ptr: *mut c_void) {
  drop(unsafe { Box::from_raw(ptr as *mut uv_prepare_t) });
}
unsafe fn dealloc_check(ptr: *mut c_void) {
  drop(unsafe { Box::from_raw(ptr as *mut uv_check_t) });
}

/// Helper: install a callback in the handle's `HandleData`.
///
/// SAFETY: `handle` must be a valid, initialized libuv handle whose `data`
/// field currently points to a `HandleData`.
unsafe fn set_callback(handle: *mut uv_handle_t, cb: impl FnMut() + 'static) {
  let data = unsafe { &mut *(uv_handle_get_data(handle) as *mut HandleData) };
  data.callback = Some(Box::new(cb));
}

/// Helper: perform the common close sequence.
///
/// 1. Drop the `HandleData` (frees user callback).
/// 2. Pack a `CloseInfo` into the handle's data slot.
/// 3. Call `uv_close` with `close_trampoline`.
/// 4. Caller must `mem::forget(self)` after this to avoid double-free.
unsafe fn do_close(
  handle: *mut uv_handle_t,
  handle_ptr: *mut c_void,
  dealloc_fn: unsafe fn(*mut c_void),
  loop_: Rc<UvLoop>,
) {
  // Free the HandleData.
  let data = unsafe { uv_handle_get_data(handle) };
  if !data.is_null() {
    drop(unsafe { Box::from_raw(data as *mut HandleData) });
  }

  let info = Box::new(CloseInfo {
    handle_ptr,
    dealloc_fn,
    _loop: loop_,
  });
  unsafe { uv_handle_set_data(handle, Box::into_raw(info) as *mut c_void) };
  unsafe { uv_close(handle, Some(close_trampoline)) };
}

// ===========================================================================
// UvIdle
// ===========================================================================

/// A libuv idle handle.  Callbacks fire once per loop iteration when the
/// handle is active.
pub struct UvIdle {
  handle: *mut uv_idle_t,
  loop_: Rc<UvLoop>,
  closed: bool,
}

unsafe extern "C" fn idle_trampoline(handle: *mut uv_idle_t) {
  let data = unsafe {
    &mut *(uv_handle_get_data(handle as *mut uv_handle_t) as *mut HandleData)
  };
  if let Some(cb) = data.callback.as_mut() {
    cb();
  }
}

impl UvIdle {
  /// Create a new idle handle on the given loop.
  pub fn new(loop_: &Rc<UvLoop>) -> Result<Self, UvError> {
    let boxed = Box::new(unsafe { std::mem::zeroed::<uv_idle_t>() });
    let raw = Box::into_raw(boxed);
    check(unsafe { uv_idle_init(loop_.as_mut_ptr(), raw) })?;
    unsafe { init_handle_data(raw as *mut uv_handle_t) };

    Ok(UvIdle {
      handle: raw,
      loop_: Rc::clone(loop_),
      closed: false,
    })
  }

  /// Wrap an already-initialized `uv_idle_t` pointer.
  ///
  /// # Safety
  ///
  /// - `handle` must point to a live, heap-allocated (`Box::into_raw`)
  ///   `uv_idle_t` that has been initialized with `uv_idle_init`.
  /// - The handle's `data` field will be overwritten — any previous value
  ///   is the caller's responsibility to clean up beforehand.
  /// - The caller transfers ownership of the handle allocation.
  pub unsafe fn from_raw(handle: *mut uv_idle_t, loop_: &Rc<UvLoop>) -> Self {
    unsafe { init_handle_data(handle as *mut uv_handle_t) };
    UvIdle {
      handle,
      loop_: Rc::clone(loop_),
      closed: false,
    }
  }

  /// Returns the raw pointer to the underlying `uv_idle_t`.
  ///
  /// The pointer is valid until `close()` is called. Use it to call
  /// [`sys`](crate::sys) functions directly.
  pub fn as_mut_ptr(&self) -> *mut uv_idle_t {
    self.handle
  }

  /// Start the idle handle with the given callback.
  pub fn start(&self, cb: impl FnMut() + 'static) -> Result<(), UvError> {
    unsafe { set_callback(self.handle as *mut uv_handle_t, cb) };
    check(unsafe { uv_idle_start(self.handle, Some(idle_trampoline)) })
  }

  /// Stop the idle handle.
  pub fn stop(&self) -> Result<(), UvError> {
    check(unsafe { uv_idle_stop(self.handle) })
  }

  /// Close the handle, releasing all resources.  Consumes `self`.
  pub fn close(mut self) {
    self.closed = true;
    unsafe {
      do_close(
        self.handle as *mut uv_handle_t,
        self.handle as *mut c_void,
        dealloc_idle,
        Rc::clone(&self.loop_),
      );
    }
    std::mem::forget(self);
  }
}

impl Drop for UvIdle {
  fn drop(&mut self) {
    if !self.closed {
      debug_assert!(
        false,
        "UvIdle dropped without calling close() — leaking to avoid UB"
      );
      // Leak the handle to avoid undefined behaviour.
    }
  }
}

// ===========================================================================
// UvTimer
// ===========================================================================

/// A libuv timer handle.
pub struct UvTimer {
  handle: *mut uv_timer_t,
  loop_: Rc<UvLoop>,
  closed: bool,
}

unsafe extern "C" fn timer_trampoline(handle: *mut uv_timer_t) {
  let data = unsafe {
    &mut *(uv_handle_get_data(handle as *mut uv_handle_t) as *mut HandleData)
  };
  if let Some(cb) = data.callback.as_mut() {
    cb();
  }
}

impl UvTimer {
  /// Create a new timer handle on the given loop.
  pub fn new(loop_: &Rc<UvLoop>) -> Result<Self, UvError> {
    let boxed = Box::new(unsafe { std::mem::zeroed::<uv_timer_t>() });
    let raw = Box::into_raw(boxed);
    check(unsafe { uv_timer_init(loop_.as_mut_ptr(), raw) })?;
    unsafe { init_handle_data(raw as *mut uv_handle_t) };

    Ok(UvTimer {
      handle: raw,
      loop_: Rc::clone(loop_),
      closed: false,
    })
  }

  /// Wrap an already-initialized `uv_timer_t` pointer.
  ///
  /// # Safety
  ///
  /// - `handle` must point to a live, heap-allocated (`Box::into_raw`)
  ///   `uv_timer_t` that has been initialized with `uv_timer_init`.
  /// - The handle's `data` field will be overwritten — any previous value
  ///   is the caller's responsibility to clean up beforehand.
  /// - The caller transfers ownership of the handle allocation.
  pub unsafe fn from_raw(handle: *mut uv_timer_t, loop_: &Rc<UvLoop>) -> Self {
    unsafe { init_handle_data(handle as *mut uv_handle_t) };
    UvTimer {
      handle,
      loop_: Rc::clone(loop_),
      closed: false,
    }
  }

  /// Returns the raw pointer to the underlying `uv_timer_t`.
  ///
  /// The pointer is valid until `close()` is called. Use it to call
  /// [`sys`](crate::sys) functions directly.
  pub fn as_mut_ptr(&self) -> *mut uv_timer_t {
    self.handle
  }

  /// Start the timer.
  ///
  /// - `timeout_ms`: initial delay in milliseconds before the first fire.
  /// - `repeat_ms`: repeat interval.  0 means fire once.
  pub fn start(
    &self,
    cb: impl FnMut() + 'static,
    timeout_ms: u64,
    repeat_ms: u64,
  ) -> Result<(), UvError> {
    unsafe { set_callback(self.handle as *mut uv_handle_t, cb) };
    check(unsafe {
      uv_timer_start(self.handle, Some(timer_trampoline), timeout_ms, repeat_ms)
    })
  }

  /// Stop the timer.
  pub fn stop(&self) -> Result<(), UvError> {
    check(unsafe { uv_timer_stop(self.handle) })
  }

  /// Restart the timer using the repeat value set with `set_repeat`.
  pub fn again(&self) -> Result<(), UvError> {
    check(unsafe { uv_timer_again(self.handle) })
  }

  /// Set the repeat interval in milliseconds.
  pub fn set_repeat(&self, repeat_ms: u64) {
    unsafe { uv_timer_set_repeat(self.handle, repeat_ms) };
  }

  /// Get the current repeat interval in milliseconds.
  pub fn repeat(&self) -> u64 {
    unsafe { uv_timer_get_repeat(self.handle) }
  }

  /// Close the handle, releasing all resources.  Consumes `self`.
  pub fn close(mut self) {
    self.closed = true;
    unsafe {
      do_close(
        self.handle as *mut uv_handle_t,
        self.handle as *mut c_void,
        dealloc_timer,
        Rc::clone(&self.loop_),
      );
    }
    std::mem::forget(self);
  }
}

impl Drop for UvTimer {
  fn drop(&mut self) {
    if !self.closed {
      debug_assert!(
        false,
        "UvTimer dropped without calling close() — leaking to avoid UB"
      );
    }
  }
}

// ===========================================================================
// UvPrepare
// ===========================================================================

/// A libuv prepare handle.  Callbacks fire once per loop iteration, before
/// I/O polling.
pub struct UvPrepare {
  handle: *mut uv_prepare_t,
  loop_: Rc<UvLoop>,
  closed: bool,
}

unsafe extern "C" fn prepare_trampoline(handle: *mut uv_prepare_t) {
  let data = unsafe {
    &mut *(uv_handle_get_data(handle as *mut uv_handle_t) as *mut HandleData)
  };
  if let Some(cb) = data.callback.as_mut() {
    cb();
  }
}

impl UvPrepare {
  /// Create a new prepare handle on the given loop.
  pub fn new(loop_: &Rc<UvLoop>) -> Result<Self, UvError> {
    let boxed = Box::new(unsafe { std::mem::zeroed::<uv_prepare_t>() });
    let raw = Box::into_raw(boxed);
    check(unsafe { uv_prepare_init(loop_.as_mut_ptr(), raw) })?;
    unsafe { init_handle_data(raw as *mut uv_handle_t) };

    Ok(UvPrepare {
      handle: raw,
      loop_: Rc::clone(loop_),
      closed: false,
    })
  }

  /// Wrap an already-initialized `uv_prepare_t` pointer.
  ///
  /// # Safety
  ///
  /// - `handle` must point to a live, heap-allocated (`Box::into_raw`)
  ///   `uv_prepare_t` that has been initialized with `uv_prepare_init`.
  /// - The handle's `data` field will be overwritten — any previous value
  ///   is the caller's responsibility to clean up beforehand.
  /// - The caller transfers ownership of the handle allocation.
  pub unsafe fn from_raw(
    handle: *mut uv_prepare_t,
    loop_: &Rc<UvLoop>,
  ) -> Self {
    unsafe { init_handle_data(handle as *mut uv_handle_t) };
    UvPrepare {
      handle,
      loop_: Rc::clone(loop_),
      closed: false,
    }
  }

  /// Returns the raw pointer to the underlying `uv_prepare_t`.
  ///
  /// The pointer is valid until `close()` is called. Use it to call
  /// [`sys`](crate::sys) functions directly.
  pub fn as_mut_ptr(&self) -> *mut uv_prepare_t {
    self.handle
  }

  /// Start the prepare handle with the given callback.
  pub fn start(&self, cb: impl FnMut() + 'static) -> Result<(), UvError> {
    unsafe { set_callback(self.handle as *mut uv_handle_t, cb) };
    check(unsafe { uv_prepare_start(self.handle, Some(prepare_trampoline)) })
  }

  /// Stop the prepare handle.
  pub fn stop(&self) -> Result<(), UvError> {
    check(unsafe { uv_prepare_stop(self.handle) })
  }

  /// Close the handle, releasing all resources.  Consumes `self`.
  pub fn close(mut self) {
    self.closed = true;
    unsafe {
      do_close(
        self.handle as *mut uv_handle_t,
        self.handle as *mut c_void,
        dealloc_prepare,
        Rc::clone(&self.loop_),
      );
    }
    std::mem::forget(self);
  }
}

impl Drop for UvPrepare {
  fn drop(&mut self) {
    if !self.closed {
      debug_assert!(
        false,
        "UvPrepare dropped without calling close() — leaking to avoid UB"
      );
    }
  }
}

// ===========================================================================
// UvCheck
// ===========================================================================

/// A libuv check handle.  Callbacks fire once per loop iteration, after
/// I/O polling.
pub struct UvCheck {
  handle: *mut uv_check_t,
  loop_: Rc<UvLoop>,
  closed: bool,
}

unsafe extern "C" fn check_trampoline(handle: *mut uv_check_t) {
  let data = unsafe {
    &mut *(uv_handle_get_data(handle as *mut uv_handle_t) as *mut HandleData)
  };
  if let Some(cb) = data.callback.as_mut() {
    cb();
  }
}

impl UvCheck {
  /// Create a new check handle on the given loop.
  pub fn new(loop_: &Rc<UvLoop>) -> Result<Self, UvError> {
    let boxed = Box::new(unsafe { std::mem::zeroed::<uv_check_t>() });
    let raw = Box::into_raw(boxed);
    check(unsafe { uv_check_init(loop_.as_mut_ptr(), raw) })?;
    unsafe { init_handle_data(raw as *mut uv_handle_t) };

    Ok(UvCheck {
      handle: raw,
      loop_: Rc::clone(loop_),
      closed: false,
    })
  }

  /// Wrap an already-initialized `uv_check_t` pointer.
  ///
  /// # Safety
  ///
  /// - `handle` must point to a live, heap-allocated (`Box::into_raw`)
  ///   `uv_check_t` that has been initialized with `uv_check_init`.
  /// - The handle's `data` field will be overwritten — any previous value
  ///   is the caller's responsibility to clean up beforehand.
  /// - The caller transfers ownership of the handle allocation.
  pub unsafe fn from_raw(handle: *mut uv_check_t, loop_: &Rc<UvLoop>) -> Self {
    unsafe { init_handle_data(handle as *mut uv_handle_t) };
    UvCheck {
      handle,
      loop_: Rc::clone(loop_),
      closed: false,
    }
  }

  /// Returns the raw pointer to the underlying `uv_check_t`.
  ///
  /// The pointer is valid until `close()` is called. Use it to call
  /// [`sys`](crate::sys) functions directly.
  pub fn as_mut_ptr(&self) -> *mut uv_check_t {
    self.handle
  }

  /// Start the check handle with the given callback.
  pub fn start(&self, cb: impl FnMut() + 'static) -> Result<(), UvError> {
    unsafe { set_callback(self.handle as *mut uv_handle_t, cb) };
    check(unsafe { uv_check_start(self.handle, Some(check_trampoline)) })
  }

  /// Stop the check handle.
  pub fn stop(&self) -> Result<(), UvError> {
    check(unsafe { uv_check_stop(self.handle) })
  }

  /// Close the handle, releasing all resources.  Consumes `self`.
  pub fn close(mut self) {
    self.closed = true;
    unsafe {
      do_close(
        self.handle as *mut uv_handle_t,
        self.handle as *mut c_void,
        dealloc_check,
        Rc::clone(&self.loop_),
      );
    }
    std::mem::forget(self);
  }
}

impl Drop for UvCheck {
  fn drop(&mut self) {
    if !self.closed {
      debug_assert!(
        false,
        "UvCheck dropped without calling close() — leaking to avoid UB"
      );
    }
  }
}
