// Copyright 2018-2026 the Deno authors. MIT license.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use deno_core::ExternalOpsTracker;

/// Idempotent ref/unref tracker for the event loop.
///
/// Wraps an `ExternalOpsTracker` and ensures that each logical
/// ref/unref pair translates to exactly one `ref_op()`/`unref_op()`
/// call, regardless of how many times `ref_()` or `unref()` is
/// called.  Safe to share across threads via `Arc`.
#[derive(Clone)]
pub(crate) struct RefTracker(Arc<RefTrackerInner>);

struct RefTrackerInner {
  ops_tracker: ExternalOpsTracker,
  refed: AtomicBool,
}

impl Drop for RefTrackerInner {
  fn drop(&mut self) {
    if *self.refed.get_mut() {
      self.ops_tracker.unref_op();
    }
  }
}

impl RefTracker {
  pub fn new(ops_tracker: ExternalOpsTracker) -> Self {
    Self(Arc::new(RefTrackerInner {
      ops_tracker,
      refed: AtomicBool::new(false),
    }))
  }

  pub fn ref_(&self) {
    if !self.0.refed.swap(true, Ordering::AcqRel) {
      self.0.ops_tracker.ref_op();
    }
  }

  pub fn unref(&self) {
    if self.0.refed.swap(false, Ordering::AcqRel) {
      self.0.ops_tracker.unref_op();
    }
  }

  pub fn is_refed(&self) -> bool {
    self.0.refed.load(Ordering::Acquire)
  }
}

pub mod blocklist;
pub mod buffer;
pub mod connection_wrap;
pub mod constant;
pub mod crypto;
pub mod dns;
pub mod fs;
pub mod handle_wrap;
pub mod http;
pub mod http2;
pub mod idna;
pub mod inspector;
pub mod ipc;
pub mod node_cli_parser;
pub mod os;
pub mod perf_hooks;
pub mod pipe_wrap;
pub mod process;
pub mod require;
pub mod sqlite;
pub mod stream_wrap;
pub mod tcp_wrap;
pub mod tls;
pub mod util;
pub mod v8;
pub mod vm;
pub mod winerror;
pub mod worker_threads;
pub mod zlib;
