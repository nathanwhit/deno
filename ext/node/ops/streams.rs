use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
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

unsafe impl Send for NodeStreamResource {}
unsafe impl Sync for NodeStreamResource {}

pub struct NodeStreamResourceInner {
  read_fn: v8::TracedReference<v8::Function>,
  write_fn: v8::TracedReference<v8::Function>,
  stream: v8::TracedReference<v8::Object>,
  context: v8::Global<v8::Context>,
  isolate: *mut v8::Isolate,
  pub(crate) readable: AtomicBool,
  pub(crate) waker: AtomicWaker,
  pub(crate) excess_buf: Mutex<ExcessBuf>,
  pub(crate) has_excess_buf: AtomicBool,
}

#[derive(Clone)]
pub struct NodeStreamResource {
  pub(crate) inner: Arc<NodeStreamResourceInner>,
}

impl std::fmt::Debug for NodeStreamResource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("NodeStreamResource")
      .field("readable", &self.inner.readable)
      .field("waker", &self.inner.waker)
      .field("has_excess_buf", &self.inner.has_excess_buf)
      .finish()
  }
}

pub(crate) struct ExcessBuf {
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

impl GarbageCollected for NodeStreamResource {
  fn trace(&self, visitor: &v8::cppgc::Visitor) {
    self.inner.trace(visitor);
  }
}
impl GarbageCollected for NodeStreamResourceInner {
  fn trace(&self, visitor: &v8::cppgc::Visitor) {
    visitor.trace(&self.read_fn);
    visitor.trace(&self.write_fn);
    visitor.trace(&self.stream);
  }
}

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
    eprintln!("new NodeStreamResource");
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
    eprintln!("made new NodeStreamResource");
    NodeStreamResource {
      inner: Arc::new(NodeStreamResourceInner {
        read_fn,
        write_fn,
        stream,
        context,
        isolate,
        readable,
        waker,
        excess_buf,
        has_excess_buf: AtomicBool::new(false),
      }),
    }
  }

  #[fast]
  pub fn wake_readable(&self) {
    eprintln!("wake_readable");
    self
      .inner
      .readable
      .store(true, std::sync::atomic::Ordering::SeqCst);
    self.inner.waker.wake();
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
    eprintln!("try_read: {:?}", buf.remaining_mut());
    let me = &self.inner;
    {
      if buf.remaining_mut() > 0
        && me
          .has_excess_buf
          .swap(false, std::sync::atomic::Ordering::SeqCst)
      {
        let mut excess_buf = me.excess_buf.lock();
        let len = excess_buf.as_slice().len();
        if len > 0 {
          let slice = excess_buf.as_slice();
          let remaining = buf.remaining_mut();
          let to_copy = std::cmp::min(len, remaining);
          buf.put_slice(&slice[..to_copy]);
          let done = excess_buf.consume(to_copy);
          me.has_excess_buf
            .store(done, std::sync::atomic::Ordering::SeqCst);
          if to_copy == remaining {
            return to_copy;
          }
        }
      }
    }

    let isolate: &mut v8::Isolate = unsafe { &mut *me.isolate };
    let scope = &mut v8::HandleScope::with_context(isolate, &me.context);
    let stream = me.stream.get(scope).unwrap();
    let read_fn = me.read_fn.get(scope).unwrap();
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
      let mut excess_buf = me.excess_buf.lock();
      excess_buf.extend_from_slice(&slice[remaining..]);
      me.has_excess_buf
        .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    len
  }

  pub fn try_write(&self, buf: &[u8]) -> usize {
    let scope = &mut self.get_scope();
    let stream = self.inner.stream.get(scope).unwrap();
    let write_fn = self.inner.write_fn.get(scope).unwrap();
    let backing_store =
      v8::ArrayBuffer::new_backing_store_from_boxed_slice(buf.into());
    let shared = v8::SharedRef::from(backing_store);
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &shared);
    let buffer = v8::Uint8Array::new(scope, buffer, 0, buf.len()).unwrap();
    let result = write_fn
      .call(scope, stream.into(), &[buffer.into()])
      .unwrap();
    if result.is_null_or_undefined() {
      return 0;
    }
    let _result = result.cast::<v8::Boolean>();
    buf.len()
  }

  fn get_scope(&self) -> v8::HandleScope {
    let isolate: &mut v8::Isolate = unsafe { &mut *self.inner.isolate };
    v8::HandleScope::with_context(isolate, &self.inner.context)
  }
}

impl AsyncRead for NodeStreamResource {
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<Result<(), std::io::Error>> {
    eprintln!("poll_read: {:?}", buf.remaining_mut());
    self.inner.waker.register(cx.waker());

    if self
      .inner
      .has_excess_buf
      .load(std::sync::atomic::Ordering::SeqCst)
      || self
        .inner
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
    let written = self.try_write(buf);
    Poll::Ready(Ok(written))
  }
  fn poll_flush(
    self: Pin<&mut Self>,
    _cx: &mut Context<'_>,
  ) -> Poll<Result<(), std::io::Error>> {
    Poll::Ready(Ok(()))
  }
  fn poll_shutdown(
    self: Pin<&mut Self>,
    _cx: &mut Context<'_>,
  ) -> Poll<Result<(), std::io::Error>> {
    Poll::Ready(Ok(()))
  }
}

//

struct StreamReq {}

pub trait StreamListener {
  fn on_stream_alloc(&self, suggested_size: usize) -> Buf;
  fn on_stream_read(&self, buf: &Buf);
  fn on_stream_after_write(&self, buf: &Buf);
  fn on_stream_close(&self);
}

pub struct Buf {
  ptr: *mut u8,
  len: usize,
}
