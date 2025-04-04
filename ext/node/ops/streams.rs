use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::task::Context;
use std::task::Poll;

use bytes::BufMut;
use deno_core::futures::task::AtomicWaker;
use deno_core::op2;
use deno_core::parking_lot::Mutex;
use deno_core::v8;
use deno_core::v8_static_strings;
use deno_core::FastStaticString;
use deno_core::GarbageCollected;
use deno_core::Resource;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
// use deno_core::cppgc::

v8_static_strings! {
  READ = "read",
  WRITE = "write"
}

fn get<'s>(
  scope: &mut v8::HandleScope<'s>,
  stream: v8::Local<'s, v8::Object>,
  prop: FastStaticString,
) -> Option<v8::Local<'s, v8::Value>> {
  let prop = prop.v8_string(scope).unwrap().into();
  stream.get(scope, prop)
}

pub struct NodeStreamResource {
  read_fn: v8::TracedReference<v8::Function>,
  write_fn: v8::TracedReference<v8::Function>,
  stream: v8::TracedReference<v8::Object>,
  context: v8::Global<v8::Context>,
  isolate: *mut v8::Isolate,
  readable: AtomicBool,
  waker: AtomicWaker,
  excess_buf: Mutex<ExcessBuf>,
  has_excess_buf: AtomicBool,
}

struct ExcessBuf {
  buf: Vec<u8>,
  index: usize,
}

impl ExcessBuf {
  pub fn new() -> Self {
    Self {
      buf: vec![],
      index: 0,
    }
  }

  pub fn consume(&mut self, len: usize) -> bool {
    if self.index + len > self.buf.len() {
      self.index = 0;
      false
    } else {
      self.index += len;
      true
    }
  }

  pub fn extend_from_slice(&mut self, slice: &[u8]) {
    self.buf.extend_from_slice(slice);
  }

  pub fn as_slice(&self) -> &[u8] {
    &self.buf[self.index..]
  }
}

impl GarbageCollected for NodeStreamResource {}

#[op2]
impl NodeStreamResource {
  #[constructor]
  #[cppgc]
  pub fn new<'a>(
    isolate: *mut v8::Isolate,
    scope: &mut v8::HandleScope<'a>,
    stream: v8::Local<'a, v8::Object>,
  ) -> NodeStreamResource {
    // stream.get(scope, READ)
    let read_fn = get(scope, stream, READ)
      .unwrap()
      .try_cast::<v8::Function>()
      .unwrap();
    let write_fn = get(scope, stream, WRITE)
      .unwrap()
      .try_cast::<v8::Function>()
      .unwrap();
    let context = scope.get_current_context();
    let context = v8::Global::new(scope, context);
    let read_fn = v8::TracedReference::new(scope, read_fn);
    let write_fn = v8::TracedReference::new(scope, write_fn);
    let stream = v8::TracedReference::new(scope, stream);
    let readable = AtomicBool::new(false);
    let waker = AtomicWaker::new();
    let excess_buf = Mutex::new(ExcessBuf::new());
    NodeStreamResource {
      read_fn,
      write_fn,
      stream,
      context,
      isolate,
      readable,
      waker,
      excess_buf,
      has_excess_buf: AtomicBool::new(false),
    }
  }

  // pub fn read(
  //   self: &Self,
  //   scope: &mut v8::HandleScope,
  //   limit: usize,
  // ) -> Result<v8::Local<v8::Object>, deno_core::error::ResourceError> {
  //   todo!()
  // }
}

impl NodeStreamResource {
  pub fn try_read(&self, mut buf: impl BufMut) -> usize {
    {
      if buf.remaining_mut() > 0
        && self
          .has_excess_buf
          .swap(false, std::sync::atomic::Ordering::SeqCst)
      {
        let mut excess_buf = self.excess_buf.lock();
        let len = excess_buf.as_slice().len();
        if len > 0 {
          let slice = excess_buf.as_slice();
          let remaining = buf.remaining_mut();
          let to_copy = std::cmp::min(len, remaining);
          buf.put_slice(&slice[..to_copy]);
          let done = excess_buf.consume(to_copy);
          self
            .has_excess_buf
            .store(done, std::sync::atomic::Ordering::SeqCst);
          if to_copy == remaining {
            return to_copy;
          }
        }
      }
    }

    let isolate: &mut v8::Isolate = unsafe { &mut *self.isolate };
    let scope = &mut v8::HandleScope::with_context(isolate, &self.context);
    let stream = self.stream.get(scope).unwrap();
    let read_fn = self.read_fn.get(scope).unwrap();
    let result = read_fn.call(scope, stream.into(), &[]).unwrap();
    if result.is_null_or_undefined() {
      return 0;
    }
    let buffer = result.cast::<v8::Uint8Array>();
    let buffer_data = buffer.data().cast::<u8>().cast_const();
    let len = buffer.byte_length();
    let slice = unsafe { std::slice::from_raw_parts(buffer_data, len) };
    let remaining = buf.remaining_mut();
    let to_copy = std::cmp::min(len, remaining);
    buf.put_slice(&slice[..to_copy]);
    if to_copy == remaining {
      return to_copy;
    }
    if len > remaining {
      let mut excess_buf = self.excess_buf.lock();
      excess_buf.extend_from_slice(&slice[remaining..]);
      self
        .has_excess_buf
        .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    len
  }
}

impl AsyncRead for NodeStreamResource {
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<Result<(), std::io::Error>> {
    self.waker.register(cx.waker());

    if self
      .has_excess_buf
      .load(std::sync::atomic::Ordering::SeqCst)
      || self
        .readable
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
      let read = self.try_read(buf);
      if read > 0 {
        return Poll::Ready(Ok(()));
      } else {
        return Poll::Pending;
      }
    } else {
      Poll::Pending
    }
  }
}

impl AsyncWrite for NodeStreamResource {
  fn poll_write(
    self: Pin<&mut Self>,
    _cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<Result<usize, std::io::Error>> {
    todo!()
  }
  fn poll_flush(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Result<(), std::io::Error>> {
    todo!()
  }
  fn poll_shutdown(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Result<(), std::io::Error>> {
    todo!()
  }
}
