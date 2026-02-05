// Copyright 2018-2026 the Deno authors. MIT license.

//! Rust bindings to libuv.
//!
//! This crate provides raw FFI bindings to libuv v1.51.0.
//! The bindings are generated using bindgen.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[cfg(test)]
mod tests {
  use super::*;
  use std::ffi::CStr;
  use std::mem::MaybeUninit;
  use std::sync::atomic::{AtomicI64, AtomicBool, Ordering};

  #[test]
  fn test_version() {
    unsafe {
      let version = uv_version();
      // libuv 1.51.0 = (1 << 16) | (51 << 8) | 0 = 78592
      assert_eq!(version, 78592);

      let version_str = uv_version_string();
      let version_cstr = CStr::from_ptr(version_str);
      assert_eq!(version_cstr.to_str().unwrap(), "1.51.0");
    }
  }

  #[test]
  fn test_loop_init_close() {
    unsafe {
      let mut loop_: MaybeUninit<uv_loop_t> = MaybeUninit::uninit();
      let result = uv_loop_init(loop_.as_mut_ptr());
      assert_eq!(result, 0);

      let loop_ = loop_.assume_init_mut();
      let result = uv_loop_close(loop_);
      assert_eq!(result, 0);
    }
  }

  #[test]
  fn test_default_loop() {
    unsafe {
      let loop_ = uv_default_loop();
      assert!(!loop_.is_null());

      // Check the loop is alive initially (no handles)
      let alive = uv_loop_alive(loop_);
      assert_eq!(alive, 0); // No handles, so not "alive"
    }
  }

  #[test]
  fn test_error_strings() {
    unsafe {
      // Test error name for ENOENT (uv_errno_t_UV_ENOENT = -2)
      let err_name = uv_err_name(uv_errno_t_UV_ENOENT);
      let err_cstr = CStr::from_ptr(err_name);
      assert_eq!(err_cstr.to_str().unwrap(), "ENOENT");

      // Test error string for ENOENT
      let err_str = uv_strerror(uv_errno_t_UV_ENOENT);
      let err_cstr = CStr::from_ptr(err_str);
      assert!(err_cstr.to_str().unwrap().contains("no such file"));
    }
  }

  /// Counter for idle callback test
  static IDLE_COUNTER: AtomicI64 = AtomicI64::new(0);

  /// Callback for idle handle - stops after 1000 iterations
  unsafe extern "C" fn idle_callback(handle: *mut uv_idle_t) {
    let count = IDLE_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    if count >= 1000 {
      unsafe { uv_idle_stop(handle) };
    }
  }

  /// Close callback - does nothing, just needed for uv_close
  unsafe extern "C" fn close_cb(_handle: *mut uv_handle_t) {}

  /// Test based on the libuv idle example from the docs
  #[test]
  fn test_idle_example() {
    IDLE_COUNTER.store(0, Ordering::SeqCst);

    unsafe {
      let mut loop_: MaybeUninit<uv_loop_t> = MaybeUninit::uninit();
      let result = uv_loop_init(loop_.as_mut_ptr());
      assert_eq!(result, 0);
      let loop_ = loop_.as_mut_ptr();

      let mut idler: MaybeUninit<uv_idle_t> = MaybeUninit::uninit();
      let result = uv_idle_init(loop_, idler.as_mut_ptr());
      assert_eq!(result, 0);

      let result = uv_idle_start(idler.as_mut_ptr(), Some(idle_callback));
      assert_eq!(result, 0);

      // Run the loop
      let result = uv_run(loop_, uv_run_mode_UV_RUN_DEFAULT);
      assert_eq!(result, 0);

      // Verify the callback ran the expected number of times
      assert_eq!(IDLE_COUNTER.load(Ordering::SeqCst), 1000);

      // Close the idle handle before closing the loop
      uv_close(idler.as_mut_ptr() as *mut uv_handle_t, Some(close_cb));
      // Run loop again to process the close
      uv_run(loop_, uv_run_mode_UV_RUN_DEFAULT);

      // Clean up
      let result = uv_loop_close(loop_);
      assert_eq!(result, 0);
    }
  }

  static TIMER_FIRED: AtomicBool = AtomicBool::new(false);

  unsafe extern "C" fn timer_callback(_handle: *mut uv_timer_t) {
    TIMER_FIRED.store(true, Ordering::SeqCst);
  }

  #[test]
  fn test_timer() {
    TIMER_FIRED.store(false, Ordering::SeqCst);

    unsafe {
      let mut loop_: MaybeUninit<uv_loop_t> = MaybeUninit::uninit();
      let result = uv_loop_init(loop_.as_mut_ptr());
      assert_eq!(result, 0);
      let loop_ = loop_.as_mut_ptr();

      let mut timer: MaybeUninit<uv_timer_t> = MaybeUninit::uninit();
      let result = uv_timer_init(loop_, timer.as_mut_ptr());
      assert_eq!(result, 0);

      // Start timer with 1ms timeout, no repeat
      let result =
        uv_timer_start(timer.as_mut_ptr(), Some(timer_callback), 1, 0);
      assert_eq!(result, 0);

      // Run the loop
      let result = uv_run(loop_, uv_run_mode_UV_RUN_DEFAULT);
      assert_eq!(result, 0);

      assert!(TIMER_FIRED.load(Ordering::SeqCst));

      // Close the timer handle before closing the loop
      uv_close(timer.as_mut_ptr() as *mut uv_handle_t, Some(close_cb));
      // Run loop again to process the close
      uv_run(loop_, uv_run_mode_UV_RUN_DEFAULT);

      let result = uv_loop_close(loop_);
      assert_eq!(result, 0);
    }
  }

  #[test]
  fn test_handle_sizes() {
    unsafe {
      // Verify handle sizes are non-zero and reasonable
      let tcp_size = uv_handle_size(uv_handle_type_UV_TCP);
      assert!(tcp_size > 0);

      let timer_size = uv_handle_size(uv_handle_type_UV_TIMER);
      assert!(timer_size > 0);

      let idle_size = uv_handle_size(uv_handle_type_UV_IDLE);
      assert!(idle_size > 0);

      let req_size = uv_req_size(uv_req_type_UV_WRITE);
      assert!(req_size > 0);
    }
  }
}
