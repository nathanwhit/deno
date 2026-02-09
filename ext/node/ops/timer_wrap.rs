// Copyright 2018-2026 the Deno authors. MIT license.

use std::ffi::c_void;

use deno_core::OpState;
use deno_core::op2;
use deno_core::v8;
use libuvrust::backend::UvHandle;
use libuvrust::backend::UvLoop;
use libuvrust::backend::UvTimer;

/// Data stored on the single libuv timer handle's `data` pointer.
/// Holds the JS `processTimers` callback and the base time for relative timestamps.
struct TimerHandleData {
  process_timers: v8::Global<v8::Function>,
  timer_base: u64,
}

/// Global timer state stored in OpState via `state.put()`.
/// Manages a single `uv_timer_t` for all JS timeouts/intervals.
pub struct NodeTimerState {
  timer: *mut UvTimer,
  // Prevent the handle data from being dropped while the timer is alive.
  _handle_data: Box<TimerHandleData>,
}

unsafe fn context_from_loop(
  loop_ptr: *mut UvLoop,
) -> Option<v8::Local<'static, v8::Context>> {
  unsafe {
    let ctx_ptr = (*loop_ptr).data;
    if ctx_ptr.is_null() {
      return None;
    }
    Some(std::mem::transmute(std::ptr::NonNull::new_unchecked(
      ctx_ptr as *mut v8::Context,
    )))
  }
}

/// C callback fired when the single native timer expires.
/// Matches Node.js `Environment::RunTimers`:
///   - Calls `processTimers(now)` from JS
///   - Return value encodes next action:
///     - 0: no more timers → unref handle
///     - >0: next expiry with ref'd timers → reschedule + ref
///     - <0: next expiry with only unref'd timers → reschedule + unref
unsafe extern "C" fn run_timers_cb(handle: *mut UvTimer) {
  unsafe {
    let data =
      libuvrust::backend::timer_handle_data(handle) as *mut TimerHandleData;
    if data.is_null() {
      return;
    }

    let loop_ptr = (*(handle as *mut UvHandle)).loop_;
    let context = match context_from_loop(loop_ptr) {
      Some(c) => c,
      None => return,
    };
    v8::callback_scope!(unsafe let scope, context);

    // Switch to explicit microtask policy so that promise continuations
    // from timer callbacks don't run during the processTimers call.
    // We need the return value from processTimers to be accurate (not
    // stale due to microtasks creating new timers). After processing
    // the return value, we restore Auto policy and run microtasks.
    scope.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);

    let now = libuvrust::backend::uv_now(loop_ptr) - (*data).timer_base;
    let now_val = v8::Number::new(scope, now as f64);

    let process_timers = (*data).process_timers.open(scope);
    let recv = v8::undefined(scope);
    let result = process_timers.call(scope, recv.into(), &[now_val.into()]);

    let ret = match result {
      Some(v) => v.number_value(scope).unwrap_or(0.0),
      None => {
        // processTimers threw an exception. Reschedule with 0ms so
        // remaining timers are processed on the next event loop tick
        // (after the uncaught exception handler runs).
        // The pending exception will propagate when we return.
        libuvrust::backend::uv_timer_start(handle, Some(run_timers_cb), 0, 0);
        libuvrust::backend::uv_handle_ref(handle as *mut UvHandle);
        scope.set_microtasks_policy(v8::MicrotasksPolicy::Auto);
        return;
      }
    };

    if ret == 0.0 {
      // No more timers -- unref the handle so event loop can exit.
      libuvrust::backend::uv_handle_unref(handle as *mut UvHandle);
    } else {
      let abs_ret = ret.abs();
      let duration = abs_ret - now as f64;
      let duration_ms = if duration > 0.0 { duration as u64 } else { 0 };

      libuvrust::backend::uv_timer_start(
        handle,
        Some(run_timers_cb),
        duration_ms,
        0,
      );

      if ret > 0.0 {
        // Has ref'd timers -- keep the handle ref'd.
        libuvrust::backend::uv_handle_ref(handle as *mut UvHandle);
      } else {
        // Only unref'd timers -- unref the handle.
        libuvrust::backend::uv_handle_unref(handle as *mut UvHandle);
      }
    }

    // Restore Auto policy and run the microtask checkpoint. This allows
    // promise continuations from timer callbacks to run, and any new
    // timers they create will correctly update the handle's ref state
    // without being overwritten by the stale return value above.
    scope.set_microtasks_policy(v8::MicrotasksPolicy::Auto);
    scope.perform_microtask_checkpoint();
  }
}

/// One-time initialization. Creates the single `uv_timer_t`, stores the
/// `processTimers` JS callback, and puts `NodeTimerState` in OpState.
/// Called lazily from JS on first timer creation.
#[op2(fast)]
pub fn op_node_timer_setup(
  scope: &mut v8::PinScope<'_, '_>,
  state: &mut OpState,
  process_timers: v8::Local<'_, v8::Function>,
) {
  if state.has::<NodeTimerState>() {
    return;
  }

  let loop_ptr = &mut *state.uv_loop as *mut UvLoop;

  let timer_ptr = unsafe {
    let timer = Box::into_raw(Box::new(libuvrust::backend::new_timer()));
    libuvrust::backend::uv_timer_init(loop_ptr, timer);
    // Start unref'd so we don't keep the event loop alive with no timers.
    libuvrust::backend::uv_handle_unref(timer as *mut UvHandle);
    timer
  };

  let timer_base = unsafe { libuvrust::backend::uv_now(loop_ptr) };
  let process_timers_global = v8::Global::new(scope, process_timers);

  let handle_data = Box::new(TimerHandleData {
    process_timers: process_timers_global,
    timer_base,
  });

  // Store a raw pointer to handle_data in the libuv handle's data field.
  let data_ptr =
    &*handle_data as *const TimerHandleData as *mut TimerHandleData;
  unsafe {
    libuvrust::backend::set_timer_handle_data(
      timer_ptr,
      data_ptr as *mut c_void,
    );
  }

  state.put(NodeTimerState {
    timer: timer_ptr,
    _handle_data: handle_data,
  });
}

/// Schedule the single native timer to fire after `duration_ms` milliseconds.
/// JS side calls this whenever a new timer is inserted that is sooner than
/// the current scheduled expiry.
#[op2(fast)]
pub fn op_node_timer_schedule(state: &mut OpState, duration_ms: f64) {
  let timer_state = state.borrow::<NodeTimerState>();
  let timer = timer_state.timer;
  let ms = if duration_ms > 0.0 {
    duration_ms as u64
  } else {
    0
  };
  unsafe {
    libuvrust::backend::uv_timer_start(timer, Some(run_timers_cb), ms, 0);
  }
  // Wake the event loop so it re-polls libuv and picks up the new timer.
  state.waker.wake();
}

/// Toggle whether the single native timer handle keeps the event loop alive.
/// JS side calls this when the ref count crosses zero.
///
/// Also notifies the external ops tracker so the event loop knows there is
/// pending work (prevents the stalled top-level-await check from firing
/// while ref'd timers are active).
#[op2(fast)]
pub fn op_node_timer_toggle_ref(state: &mut OpState, is_ref: bool) {
  let timer_state = state.borrow::<NodeTimerState>();
  let timer = timer_state.timer;
  unsafe {
    if is_ref {
      libuvrust::backend::uv_handle_ref(timer as *mut UvHandle);
    } else {
      libuvrust::backend::uv_handle_unref(timer as *mut UvHandle);
    }
  }
  if is_ref {
    state.external_ops_tracker.ref_op();
  } else {
    state.external_ops_tracker.unref_op();
  }
}

/// Returns the current time relative to timer initialization, in milliseconds.
/// This is the equivalent of `uv_now(loop) - timer_base`.
#[op2(fast)]
pub fn op_node_timer_now(state: &mut OpState) -> f64 {
  // Copy timer_base first so the borrow of gotham_state is released
  // before we access uv_loop.
  let timer_base = state.borrow::<NodeTimerState>()._handle_data.timer_base;
  let loop_ptr = &*state.uv_loop as *const UvLoop;
  let now = unsafe { libuvrust::backend::uv_now(loop_ptr) };
  (now - timer_base) as f64
}
