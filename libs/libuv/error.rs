// Copyright 2018-2026 the Deno authors. MIT license.

use std::ffi::CStr;
use std::fmt;

/// An error returned by a libuv function.
///
/// Wraps a negative libuv error code (e.g. `UV_ENOENT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UvError(i32);

impl UvError {
  /// Returns the raw libuv error code (always negative).
  pub fn code(&self) -> i32 {
    self.0
  }

  /// Returns the error name (e.g. `"ENOENT"`).
  pub fn name(&self) -> &str {
    // SAFETY: uv_err_name returns a static string for known error codes.
    unsafe {
      let ptr = crate::sys::uv_err_name(self.0);
      CStr::from_ptr(ptr).to_str().unwrap_or("unknown")
    }
  }

  /// Returns a human-readable error message.
  pub fn message(&self) -> &str {
    // SAFETY: uv_strerror returns a static string for known error codes.
    unsafe {
      let ptr = crate::sys::uv_strerror(self.0);
      CStr::from_ptr(ptr).to_str().unwrap_or("unknown error")
    }
  }
}

impl fmt::Display for UvError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}: {}", self.name(), self.message())
  }
}

impl std::error::Error for UvError {}

/// Check a libuv return code. Returns `Err(UvError)` if negative.
pub(crate) fn check(rc: i32) -> Result<(), UvError> {
  if rc < 0 { Err(UvError(rc)) } else { Ok(()) }
}
