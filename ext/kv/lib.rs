// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

pub mod dynamic;
mod interface;
pub mod remote;
pub mod sqlite;

use std::borrow::Cow;
use std::cell::RefCell;
use std::convert::Infallible;
use std::future::Future;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Duration;

use base64::prelude::BASE64_URL_SAFE;
use base64::Engine;
use chrono::DateTime;
use chrono::Utc;
use convert_util::any_err;
use convert_util::transmute_vec;
use convert_util::OptionNull;
use convert_util::SerdeWrapper;
use convert_util::TupleArray;
use deno_core::anyhow::Context;
use deno_core::convert::Smi;
use deno_core::error::get_custom_error_class;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::error::StdAnyError;
use deno_core::futures::StreamExt;
use deno_core::op2;
use deno_core::serde_v8::BigInt;
use deno_core::v8;
use deno_core::AsyncRefCell;
use deno_core::ByteString;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::FromV8;
use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::ToJsBuffer;
use deno_core::ToV8;
use denokv_proto::decode_key;
use denokv_proto::encode_key;
use denokv_proto::AtomicWrite;
use denokv_proto::Check;
use denokv_proto::Consistency;
use denokv_proto::Database;
use denokv_proto::Enqueue;
use denokv_proto::Key;
use denokv_proto::KeyPart;
use denokv_proto::KvEntry;
use denokv_proto::KvValue;
use denokv_proto::Mutation;
use denokv_proto::MutationKind;
use denokv_proto::QueueMessageHandle;
use denokv_proto::ReadRange;
use denokv_proto::SnapshotReadOptions;
use denokv_proto::WatchKeyOutput;
use denokv_proto::WatchStream;
use log::debug;
use serde::Deserialize;
use serde::Serialize;

use crate::convert_util::VecArray;
pub use crate::interface::*;

pub const UNSTABLE_FEATURE_NAME: &str = "kv";

const MAX_WRITE_KEY_SIZE_BYTES: usize = 2048;
// range selectors can contain 0x00 or 0xff suffixes
const MAX_READ_KEY_SIZE_BYTES: usize = MAX_WRITE_KEY_SIZE_BYTES + 1;
const MAX_VALUE_SIZE_BYTES: usize = 65536;
const MAX_READ_RANGES: usize = 10;
const MAX_READ_ENTRIES: usize = 1000;
const MAX_CHECKS: usize = 100;
const MAX_MUTATIONS: usize = 1000;
const MAX_WATCHED_KEYS: usize = 10;
const MAX_TOTAL_MUTATION_SIZE_BYTES: usize = 800 * 1024;
const MAX_TOTAL_KEY_SIZE_BYTES: usize = 80 * 1024;

deno_core::extension!(deno_kv,
  deps = [ deno_console, deno_web ],
  parameters = [ DBH: DatabaseHandler ],
  ops = [
    op_kv_database_open<DBH>,
    op_kv_snapshot_read<DBH>,
    op_kv_atomic_write<DBH>,
    op_kv_encode_cursor,
    op_kv_dequeue_next_message<DBH>,
    op_kv_finish_dequeued_message<DBH>,
    op_kv_watch<DBH>,
    op_kv_watch_next,
  ],
  esm = [ "01_db.ts" ],
  options = {
    handler: DBH,
  },
  state = |state, options| {
    state.put(Rc::new(options.handler));
  }
);

struct DatabaseResource<DB: Database + 'static> {
  db: DB,
  cancel_handle: Rc<CancelHandle>,
}

impl<DB: Database + 'static> Resource for DatabaseResource<DB> {
  fn name(&self) -> Cow<str> {
    "database".into()
  }

  fn close(self: Rc<Self>) {
    self.db.close();
    self.cancel_handle.cancel();
  }
}

struct DatabaseWatcherResource {
  stream: AsyncRefCell<WatchStream>,
  db_cancel_handle: Rc<CancelHandle>,
  cancel_handle: Rc<CancelHandle>,
}

impl Resource for DatabaseWatcherResource {
  fn name(&self) -> Cow<str> {
    "databaseWatcher".into()
  }

  fn close(self: Rc<Self>) {
    self.cancel_handle.cancel()
  }
}

#[op2(async)]
#[smi]
async fn op_kv_database_open<DBH>(
  state: Rc<RefCell<OpState>>,
  #[string] path: Option<String>,
) -> Result<ResourceId, AnyError>
where
  DBH: DatabaseHandler + 'static,
{
  let handler = {
    let state = state.borrow();
    // TODO(bartlomieju): replace with `state.feature_checker.check_or_exit`
    // once we phase out `check_or_exit_with_legacy_fallback`
    state
      .feature_checker
      .check_or_exit_with_legacy_fallback(UNSTABLE_FEATURE_NAME, "Deno.openKv");
    state.borrow::<Rc<DBH>>().clone()
  };
  let db = handler.open(state.clone(), path).await?;
  let rid = state.borrow_mut().resource_table.add(DatabaseResource {
    db,
    cancel_handle: CancelHandle::new_rc(),
  });
  Ok(rid)
}

type KvKey = Vec<KeyPart>;

type V8KvKey = VecArray<KeyPartWrapper>;

fn to_v8_infallible<'a, T>(
  value: T,
  scope: &mut v8::HandleScope<'a>,
) -> v8::Local<'a, v8::Value>
where
  T: ToV8<'a, Error = Infallible>,
{
  match value.to_v8(scope) {
    Ok(value) => value,
    Err(never) => match never {},
  }
}

wrapper_struct! {
  struct ToV8KvEntry(KvEntry);
  struct BigIntWrapper(num_bigint::BigInt);
  pub struct KeyPartWrapper(KeyPart);
  pub struct KvValueWrapper(KvValue);
  struct V8KvCheck(Check);
  struct V8ReadRange(ReadRange);

  struct V8KvMutation(Mutation);
}

impl<'a> ToV8<'a> for BigIntWrapper {
  type Error = Infallible;
  fn to_v8(
    self,
    scope: &mut v8::HandleScope<'a>,
  ) -> Result<v8::Local<'a, v8::Value>, Self::Error> {
    let (sign, words) = self.0.to_u64_digits();
    let sign_bit = sign == num_bigint::Sign::Minus;
    Ok(
      v8::BigInt::new_from_words(scope, sign_bit, &words)
        .unwrap()
        .into(),
    )
  }
}

impl<'a> FromV8<'a> for BigIntWrapper {
  type Error = StdAnyError;

  fn from_v8(
    _scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    let v8bigint = v8::Local::<v8::BigInt>::try_from(value).map_err(|_| {
      anyhow::anyhow!("Expected bigint, got {}", value.type_repr())
    })?;
    let word_count = v8bigint.word_count();
    let mut words: smallvec::SmallVec<[u64; 1]> =
      smallvec::smallvec![0u64; word_count];
    let (sign_bit, _words) = v8bigint.to_words_array(&mut words);
    let sign = match sign_bit {
      true => num_bigint::Sign::Minus,
      false => num_bigint::Sign::Plus,
    };
    // SAFETY: Because the alignment of u64 is 8, the alignment of u32 is 4, and
    // the size of u64 is 8, the size of u32 is 4, the alignment of u32 is a
    // factor of the alignment of u64, and the size of u32 is a factor of the
    // size of u64, we can safely transmute the slice of u64 to a slice of u32.
    let (prefix, slice, suffix) = unsafe { words.align_to::<u32>() };
    assert!(prefix.is_empty());
    assert!(suffix.is_empty());
    assert_eq!(slice.len(), words.len() * 2);
    let big_int = num_bigint::BigInt::from_slice(sign, slice);
    Ok(Self(big_int))
  }
}

impl<'a> ToV8<'a> for KeyPartWrapper {
  type Error = StdAnyError;
  fn to_v8(
    self,
    scope: &mut v8::HandleScope<'a>,
  ) -> Result<v8::Local<'a, v8::Value>, Self::Error> {
    match self.0 {
      KeyPart::False => Ok(to_v8_infallible(false, scope)),
      KeyPart::True => Ok(to_v8_infallible(true, scope)),
      KeyPart::Float(n) => {
        Ok(to_v8_infallible(deno_core::convert::Number(n), scope))
      }
      KeyPart::Int(n) => Ok(to_v8_infallible(BigIntWrapper(n), scope)),
      KeyPart::String(s) => {
        v8::String::new(scope, &s).map(Into::into).ok_or_else(|| {
          anyhow::anyhow!(
            "Cannot allocate String: buffer exceeds maximum length."
          )
          .into()
        })
      }
      KeyPart::Bytes(buf) => {
        let buf = buf.into_boxed_slice();
        if buf.is_empty() {
          let ab = v8::ArrayBuffer::new(scope, 0);
          return Ok(
            v8::Uint8Array::new(scope, ab, 0, 0)
              .expect("Failed to create Uint8Array")
              .into(),
          );
        }
        let buf_len: usize = buf.len();
        let backing_store =
          v8::ArrayBuffer::new_backing_store_from_boxed_slice(buf);
        let backing_store_shared = backing_store.make_shared();
        let ab =
          v8::ArrayBuffer::with_backing_store(scope, &backing_store_shared);
        Ok(
          v8::Uint8Array::new(scope, ab, 0, buf_len)
            .expect("Failed to create Uint8Array")
            .into(),
        )
      }
    }
  }
}

impl<'a> FromV8<'a> for KeyPartWrapper {
  type Error = StdAnyError;

  fn from_v8(
    scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    if value.is_boolean() {
      if value.boolean_value(scope) {
        Ok(KeyPart::True.into())
      } else {
        Ok(KeyPart::False.into())
      }
    } else if value.is_number() {
      Ok(KeyPart::Float(value.number_value(scope).unwrap()).into())
    } else if value.is_big_int() {
      Ok(KeyPart::Int(BigIntWrapper::from_v8(scope, value)?.into()).into())
    } else if value.is_string() {
      Ok(
        KeyPart::String(deno_core::serde_v8::to_utf8(
          v8::Local::<v8::String>::try_from(value).unwrap(),
          scope,
        ))
        .into(),
      )
    } else if value.is_uint8_array() || value.is_array_buffer_view() {
      Ok(
        KeyPart::Bytes(
          deno_core::serde_v8::from_v8::<JsBuffer>(scope, value)
            .map_err(any_err)?
            .to_vec(),
        )
        .into(),
      )
    } else {
      Err(
        anyhow::anyhow!(
          "expected string, number, bigint, ArrayBufferView, boolean, got {}",
          value.type_repr()
        )
        .into(),
      )
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum FromV8Value {
  V8(JsBuffer),
  Bytes(JsBuffer),
  U64(BigInt),
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum ToV8Value {
  V8(ToJsBuffer),
  Bytes(ToJsBuffer),
  U64(BigInt),
}

impl TryFrom<FromV8Value> for KvValue {
  type Error = AnyError;
  fn try_from(value: FromV8Value) -> Result<Self, AnyError> {
    Ok(match value {
      FromV8Value::V8(buf) => KvValue::V8(buf.to_vec()),
      FromV8Value::Bytes(buf) => KvValue::Bytes(buf.to_vec()),
      FromV8Value::U64(n) => {
        KvValue::U64(num_bigint::BigInt::from(n).try_into()?)
      }
    })
  }
}

impl From<KvValue> for ToV8Value {
  fn from(value: KvValue) -> Self {
    match value {
      KvValue::V8(buf) => ToV8Value::V8(buf.into()),
      KvValue::Bytes(buf) => ToV8Value::Bytes(buf.into()),
      KvValue::U64(n) => ToV8Value::U64(num_bigint::BigInt::from(n).into()),
    }
  }
}

impl<'a> ToV8<'a> for ToV8KvEntry {
  type Error = StdAnyError;

  fn to_v8(
    self,
    scope: &mut v8::HandleScope<'a>,
  ) -> Result<v8::Local<'a, v8::Value>, Self::Error> {
    let Self(entry) = self;
    let key = deno_core::ascii_str!("key").v8_string(scope).into();
    let value = deno_core::ascii_str!("value").v8_string(scope).into();
    let versionstamp = deno_core::ascii_str!("versionstamp")
      .v8_string(scope)
      .into();
    let key_v = VecArray(
      decode_key(&entry.key)
        .map_err(any_err)?
        .0
        .into_iter()
        .map(KeyPartWrapper)
        .collect::<Vec<_>>(),
    )
    .to_v8(scope)?;
    let value_v = ToV8Value::try_from(entry.value).map_err(any_err)?;
    let value_v =
      deno_core::serde_v8::to_v8(scope, value_v).map_err(any_err)?;
    let versionstamp_v = deno_core::serde_v8::to_v8(
      scope,
      &ByteString::from(faster_hex::hex_string(&entry.versionstamp)),
    )
    .map_err(any_err)?;
    let null = v8::null(scope).into();

    let obj = v8::Object::with_prototype_and_properties(
      scope,
      null,
      &[key, value, versionstamp],
      &[key_v, value_v, versionstamp_v],
    );
    Ok(obj.into())
  }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum V8Consistency {
  Strong,
  Eventual,
}

impl From<V8Consistency> for Consistency {
  fn from(value: V8Consistency) -> Self {
    match value {
      V8Consistency::Strong => Consistency::Strong,
      V8Consistency::Eventual => Consistency::Eventual,
    }
  }
}

fn cast_keyparts(parts: VecArray<KeyPartWrapper>) -> Vec<KeyPart> {
  transmute_vec(parts.0)
}

impl<'a> FromV8<'a> for V8ReadRange {
  type Error = StdAnyError;

  fn from_v8(
    scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    let convert_util::TupleArray((
      prefix,
      start,
      end,
      Smi(limit),
      reverse,
      SerdeWrapper(cursor),
    )): convert_util::TupleArray<(
      OptionNull<V8KvKey>,
      OptionNull<V8KvKey>,
      OptionNull<V8KvKey>,
      Smi<u32>,
      bool,
      SerdeWrapper<Option<ByteString>>,
    )> = convert_util::TupleArray::from_v8(scope, value)?;

    let prefix = prefix.0.map(cast_keyparts);
    let start = start.0.map(cast_keyparts);
    let end = end.0.map(cast_keyparts);

    let selector = RawSelector::from_tuple(prefix, start, end, scope)?;

    let (start, end) =
      decode_selector_and_cursor(&selector, reverse, cursor.as_ref())?;
    check_read_key_size(&start)?;
    check_read_key_size(&end)?;

    Ok(
      ReadRange {
        start,
        end,
        limit: NonZeroU32::new(limit)
          .with_context(|| "limit must be greater than 0")?,
        reverse,
      }
      .into(),
    )
  }
}

#[op2(async)]
#[to_v8]
fn op_kv_snapshot_read<DBH>(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  #[from_v8] VecArray(ranges): VecArray<V8ReadRange>,
  #[serde] consistency: V8Consistency,
) -> Result<
  impl Future<Output = Result<VecArray<VecArray<ToV8KvEntry>>, AnyError>>,
  AnyError,
>
where
  DBH: DatabaseHandler + 'static,
{
  let db = {
    let state = state.borrow();
    let resource =
      state.resource_table.get::<DatabaseResource<DBH::DB>>(rid)?;
    resource.db.clone()
  };

  if ranges.len() > MAX_READ_RANGES {
    return Err(type_error(format!(
      "too many ranges (max {})",
      MAX_READ_RANGES
    )));
  }

  let mut total_entries = 0usize;

  let read_ranges = transmute_vec(ranges);

  read_ranges.iter().for_each(|c| {
    total_entries += c.limit.get() as usize;
  });

  if total_entries > MAX_READ_ENTRIES {
    return Err(type_error(format!(
      "too many entries (max {})",
      MAX_READ_ENTRIES
    )));
  }

  let opts = SnapshotReadOptions {
    consistency: consistency.into(),
  };
  Ok(async move {
    let output_ranges = db.snapshot_read(read_ranges, opts).await?;
    let output_ranges = output_ranges
      .into_iter()
      .map(|x| {
        VecArray(
          x.entries
            .into_iter()
            .map(ToV8KvEntry::from)
            .collect::<Vec<_>>(),
        )
      })
      .collect::<Vec<_>>()
      .into();

    Ok(output_ranges)
  })
}

struct QueueMessageResource<QPH: QueueMessageHandle + 'static> {
  handle: QPH,
}

impl<QMH: QueueMessageHandle + 'static> Resource for QueueMessageResource<QMH> {
  fn name(&self) -> Cow<str> {
    "queueMessage".into()
  }
}

#[op2(async)]
#[serde]
async fn op_kv_dequeue_next_message<DBH>(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<Option<(ToJsBuffer, ResourceId)>, AnyError>
where
  DBH: DatabaseHandler + 'static,
{
  let db = {
    let state = state.borrow();
    let resource =
      match state.resource_table.get::<DatabaseResource<DBH::DB>>(rid) {
        Ok(resource) => resource,
        Err(err) => {
          if get_custom_error_class(&err) == Some("BadResource") {
            return Ok(None);
          } else {
            return Err(err);
          }
        }
      };
    resource.db.clone()
  };

  let Some(mut handle) = db.dequeue_next_message().await? else {
    return Ok(None);
  };
  let payload = handle.take_payload().await?.into();
  let handle_rid = {
    let mut state = state.borrow_mut();
    state.resource_table.add(QueueMessageResource { handle })
  };
  Ok(Some((payload, handle_rid)))
}

#[op2]
#[smi]
fn op_kv_watch<DBH>(
  scope: &mut v8::HandleScope,
  state: &mut OpState,
  #[smi] rid: ResourceId,
  #[from_v8] VecArray(keys): VecArray<V8KvKey>,
) -> Result<ResourceId, AnyError>
where
  DBH: DatabaseHandler + 'static,
{
  let resource = state.resource_table.get::<DatabaseResource<DBH::DB>>(rid)?;

  if keys.len() > MAX_WATCHED_KEYS {
    return Err(type_error(format!(
      "too many keys (max {})",
      MAX_WATCHED_KEYS
    )));
  }

  let keys: Vec<Vec<u8>> = keys
    .into_iter()
    .map(|k| encode_v8_key(cast_keyparts(k), scope))
    .collect::<Result<_, _>>()?;

  for k in &keys {
    check_read_key_size(k)?;
  }

  let stream = resource.db.watch(keys);

  let rid = state.resource_table.add(DatabaseWatcherResource {
    stream: AsyncRefCell::new(stream),
    db_cancel_handle: resource.cancel_handle.clone(),
    cancel_handle: CancelHandle::new_rc(),
  });

  Ok(rid)
}

enum WatchEntry {
  Changed(Option<ToV8KvEntry>),
  Unchanged,
}

impl<'a> ToV8<'a> for WatchEntry {
  type Error = StdAnyError;

  fn to_v8(
    self,
    scope: &mut v8::HandleScope<'a>,
  ) -> Result<v8::Local<'a, v8::Value>, Self::Error> {
    match self {
      WatchEntry::Changed(Some(entry)) => entry.to_v8(scope),
      WatchEntry::Changed(None) | WatchEntry::Unchanged => {
        Ok(v8::null(scope).into())
      }
    }
  }
}

#[path = "./convert.rs"]
mod convert_util;

#[derive(Serialize)]
struct Foo(Option<i32>);

#[op2(async)]
#[to_v8]
async fn op_kv_watch_next(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<OptionNull<VecArray<WatchEntry>>, AnyError> {
  let resource = {
    let state = state.borrow();
    let resource = state.resource_table.get::<DatabaseWatcherResource>(rid)?;
    resource.clone()
  };

  let db_cancel_handle = resource.db_cancel_handle.clone();
  let cancel_handle = resource.cancel_handle.clone();
  let stream = RcRef::map(resource, |r| &r.stream)
    .borrow_mut()
    .or_cancel(db_cancel_handle.clone())
    .or_cancel(cancel_handle.clone())
    .await;
  let Ok(Ok(mut stream)) = stream else {
    return Ok(None.into());
  };

  // We hold a strong reference to `resource`, so we can't rely on the stream
  // being dropped when the db connection is closed
  let Ok(Ok(Some(res))) = stream
    .next()
    .or_cancel(db_cancel_handle)
    .or_cancel(cancel_handle)
    .await
  else {
    return Ok(None.into());
  };

  let entries = res?;
  let entries = entries
    .into_iter()
    .map(|entry| {
      Ok(match entry {
        WatchKeyOutput::Changed { entry } => {
          WatchEntry::Changed(entry.map(TryInto::try_into).transpose()?)
        }
        WatchKeyOutput::Unchanged => WatchEntry::Unchanged,
      })
    })
    .collect::<Result<Vec<_>, anyhow::Error>>()?
    .into();

  Ok(Some(entries).into())
}

#[op2(async)]
async fn op_kv_finish_dequeued_message<DBH>(
  state: Rc<RefCell<OpState>>,
  #[smi] handle_rid: ResourceId,
  success: bool,
) -> Result<(), AnyError>
where
  DBH: DatabaseHandler + 'static,
{
  let handle = {
    let mut state = state.borrow_mut();
    let handle = state
      .resource_table
      .take::<QueueMessageResource<<<DBH>::DB as Database>::QMH>>(handle_rid)
      .map_err(|_| type_error("Queue message not found"))?;
    Rc::try_unwrap(handle)
      .map_err(|_| type_error("Queue message not found"))?
      .handle
  };
  // if we fail to finish the message, there is not much we can do and the
  // message will be retried anyway, so we just ignore the error
  if let Err(err) = handle.finish(success).await {
    debug!("Failed to finish dequeued message: {}", err);
  };
  Ok(())
}

impl<'a> FromV8<'a> for V8KvCheck {
  type Error = StdAnyError;
  fn from_v8(
    scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    let convert_util::TupleArray((kv_key, SerdeWrapper(versionstamp))): TupleArray<(VecArray<KeyPartWrapper>,  SerdeWrapper<Option<ByteString>>)> =
      convert_util::TupleArray::from_v8(scope, value)?;

    let versionstamp = match versionstamp {
      None => None,
      Some(data) => {
        let mut out = [0u8; 10];
        if data.len() != 20 {
          return Err(type_error("invalid versionstamp length").into());
        }
        faster_hex::hex_decode(&data, &mut out)
          .map_err(|_| type_error("invalid versionstamp"))?;
        Some(out)
      }
    };

    Ok(Self(Check {
      key: encode_v8_key(cast_keyparts(kv_key), scope)?,
      versionstamp,
    }))
  }
}

impl<'a> FromV8<'a> for V8KvMutation {
  type Error = StdAnyError;

  fn from_v8(
    scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    let TupleArray((key, kind, value, expire_in)): TupleArray<(
      V8KvKey,
      SerdeWrapper<String>,
      SerdeWrapper<Option<FromV8Value>>,
      SerdeWrapper<Option<u64>>,
    )> = TupleArray::from_v8(scope, value)?;
    let kind = kind.0;
    let value = value.0;
    let expire_in = expire_in.0;
    let current_timestamp = utc_now();
    let key = encode_v8_key(cast_keyparts(key), scope)?;
    let kind = match (kind.as_str(), value) {
      ("set", Some(value)) => MutationKind::Set(value.try_into()?),
      ("delete", None) => MutationKind::Delete,
      ("sum", Some(value)) => MutationKind::Sum(value.try_into()?),
      ("min", Some(value)) => MutationKind::Min(value.try_into()?),
      ("max", Some(value)) => MutationKind::Max(value.try_into()?),
      ("setSuffixVersionstampedKey", Some(value)) => {
        MutationKind::SetSuffixVersionstampedKey(value.try_into()?)
      }
      (op, Some(_)) => {
        return Err(
          type_error(format!("invalid mutation '{op}' with value")).into(),
        )
      }
      (op, None) => {
        return Err(
          type_error(format!("invalid mutation '{op}' without value")).into(),
        )
      }
    };
    Ok(
      Mutation {
        key,
        kind,
        expire_at: expire_in.map(|expire_in| {
          current_timestamp + Duration::from_millis(expire_in)
        }),
      }
      .into(),
    )
  }
}

struct V8Enqueue(JsBuffer, u64, Vec<KvKey>, Option<Vec<u32>>);

impl<'a> FromV8<'a> for V8Enqueue {
  type Error = StdAnyError;

  fn from_v8(
    scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    let TupleArray((
      payload,
      deadline,
      keys,
      backoff_schedule,
    )): TupleArray<(
      SerdeWrapper<JsBuffer>,
      SerdeWrapper<u64>,
      VecArray<V8KvKey>,
      OptionNull<VecArray<Smi<u32>>>,
    )> = TupleArray::from_v8(scope, value)?;

    Ok(V8Enqueue(
      payload.0,
      deadline.0,
      keys.0.into_iter().map(cast_keyparts).collect(),
      backoff_schedule.0.map(|v| v.0.into_iter().map(|v| v.0).collect()),
    ))
  }
}

// type V8Enqueue2 =

fn enqueue_from_v8(
  value: V8Enqueue,
  current_timestamp: DateTime<Utc>,
  scope: &mut v8::HandleScope,
) -> Result<Enqueue, AnyError> {
  Ok(Enqueue {
    payload: value.0.to_vec(),
    deadline: current_timestamp
      + chrono::Duration::milliseconds(value.1 as i64),
    keys_if_undelivered: value
      .2
      .into_iter()
      .map(|k| encode_v8_key(k, scope))
      .collect::<Result<_, _>>()?,
    backoff_schedule: value.3,
  })
}

fn encode_v8_key<'s>(
  key: KvKey,
  _scope: &mut v8::HandleScope<'s>,
) -> Result<Vec<u8>, AnyError> {
  encode_key(&Key(key)).map_err(Into::into)
}

enum RawSelector {
  Prefixed {
    prefix: Vec<u8>,
    start: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
  },
  Range {
    start: Vec<u8>,
    end: Vec<u8>,
  },
}

impl RawSelector {
  fn from_tuple(
    prefix: Option<KvKey>,
    start: Option<KvKey>,
    end: Option<KvKey>,
    scope: &mut v8::HandleScope,
  ) -> Result<Self, AnyError> {
    let prefix = prefix.map(|k| encode_v8_key(k, scope)).transpose()?;
    let start = start.map(|k| encode_v8_key(k, scope)).transpose()?;
    let end = end.map(|k| encode_v8_key(k, scope)).transpose()?;

    match (prefix, start, end) {
      (Some(prefix), None, None) => Ok(Self::Prefixed {
        prefix,
        start: None,
        end: None,
      }),
      (Some(prefix), Some(start), None) => {
        if !start.starts_with(&prefix) || start.len() == prefix.len() {
          return Err(type_error(
            "start key is not in the keyspace defined by prefix",
          ));
        }
        Ok(Self::Prefixed {
          prefix,
          start: Some(start),
          end: None,
        })
      }
      (Some(prefix), None, Some(end)) => {
        if !end.starts_with(&prefix) || end.len() == prefix.len() {
          return Err(type_error(
            "end key is not in the keyspace defined by prefix",
          ));
        }
        Ok(Self::Prefixed {
          prefix,
          start: None,
          end: Some(end),
        })
      }
      (None, Some(start), Some(end)) => {
        if start > end {
          return Err(type_error("start key is greater than end key"));
        }
        Ok(Self::Range { start, end })
      }
      (None, Some(start), None) => {
        let end = start.iter().copied().chain(Some(0)).collect();
        Ok(Self::Range { start, end })
      }
      _ => Err(type_error("invalid range")),
    }
  }

  fn start(&self) -> Option<&[u8]> {
    match self {
      Self::Prefixed { start, .. } => start.as_deref(),
      Self::Range { start, .. } => Some(start),
    }
  }

  fn end(&self) -> Option<&[u8]> {
    match self {
      Self::Prefixed { end, .. } => end.as_deref(),
      Self::Range { end, .. } => Some(end),
    }
  }

  fn common_prefix(&self) -> &[u8] {
    match self {
      Self::Prefixed { prefix, .. } => prefix,
      Self::Range { start, end } => common_prefix_for_bytes(start, end),
    }
  }

  fn range_start_key(&self) -> Vec<u8> {
    match self {
      Self::Prefixed {
        start: Some(start), ..
      } => start.clone(),
      Self::Range { start, .. } => start.clone(),
      Self::Prefixed { prefix, .. } => {
        prefix.iter().copied().chain(Some(0)).collect()
      }
    }
  }

  fn range_end_key(&self) -> Vec<u8> {
    match self {
      Self::Prefixed { end: Some(end), .. } => end.clone(),
      Self::Range { end, .. } => end.clone(),
      Self::Prefixed { prefix, .. } => {
        prefix.iter().copied().chain(Some(0xff)).collect()
      }
    }
  }
}

fn common_prefix_for_bytes<'a>(a: &'a [u8], b: &'a [u8]) -> &'a [u8] {
  let mut i = 0;
  while i < a.len() && i < b.len() && a[i] == b[i] {
    i += 1;
  }
  &a[..i]
}

fn encode_cursor(
  selector: &RawSelector,
  boundary_key: &[u8],
) -> Result<String, AnyError> {
  let common_prefix = selector.common_prefix();
  if !boundary_key.starts_with(common_prefix) {
    return Err(type_error("invalid boundary key"));
  }
  Ok(BASE64_URL_SAFE.encode(&boundary_key[common_prefix.len()..]))
}

fn decode_selector_and_cursor(
  selector: &RawSelector,
  reverse: bool,
  cursor: Option<&ByteString>,
) -> Result<(Vec<u8>, Vec<u8>), AnyError> {
  let Some(cursor) = cursor else {
    return Ok((selector.range_start_key(), selector.range_end_key()));
  };

  let common_prefix = selector.common_prefix();
  let cursor = BASE64_URL_SAFE
    .decode(cursor)
    .map_err(|_| type_error("invalid cursor"))?;

  let first_key: Vec<u8>;
  let last_key: Vec<u8>;

  if reverse {
    first_key = selector.range_start_key();
    last_key = common_prefix
      .iter()
      .copied()
      .chain(cursor.iter().copied())
      .collect();
  } else {
    first_key = common_prefix
      .iter()
      .copied()
      .chain(cursor.iter().copied())
      .chain(Some(0))
      .collect();
    last_key = selector.range_end_key();
  }

  // Defend against out-of-bounds reading
  if let Some(start) = selector.start() {
    if &first_key[..] < start {
      return Err(type_error("cursor out of bounds"));
    }
  }

  if let Some(end) = selector.end() {
    if &last_key[..] > end {
      return Err(type_error("cursor out of bounds"));
    }
  }

  Ok((first_key, last_key))
}

#[op2(async)]
#[string]
fn op_kv_atomic_write<'s, DBH>(
  scope: &mut v8::HandleScope<'s>,
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  // [any[], bytestring ? undefined][]
  #[from_v8] VecArray(checks): VecArray<V8KvCheck>,
  #[from_v8] VecArray(mutations): VecArray<V8KvMutation>,
  #[from_v8] VecArray(enqueues): VecArray<V8Enqueue>,
) -> Result<impl Future<Output = Result<Option<String>, AnyError>>, AnyError>
where
  DBH: DatabaseHandler + 'static,
{
  let current_timestamp = chrono::Utc::now();
  let db = {
    let state = state.borrow();
    let resource =
      state.resource_table.get::<DatabaseResource<DBH::DB>>(rid)?;
    resource.db.clone()
  };

  if checks.len() > MAX_CHECKS {
    return Err(type_error(format!("too many checks (max {})", MAX_CHECKS)));
  }

  if mutations.len() + enqueues.len() > MAX_MUTATIONS {
    return Err(type_error(format!(
      "too many mutations (max {})",
      MAX_MUTATIONS
    )));
  }

  let mutations = transmute_vec(mutations);
  let enqueues = enqueues
    .into_iter()
    .map(|e| enqueue_from_v8(e, current_timestamp, scope))
    .collect::<Result<Vec<Enqueue>, AnyError>>()
    .with_context(|| "invalid enqueue")?;

  let mut total_payload_size = 0usize;
  let mut total_key_size = 0usize;

  for key in checks
    .iter()
    .map(|c| &c.0.key)
    .chain(mutations.iter().map(|m| &m.key))
  {
    if key.is_empty() {
      return Err(type_error("key cannot be empty"));
    }

    total_payload_size += check_write_key_size(key)?;
  }

  for (key, value) in mutations
    .iter()
    .flat_map(|m| m.kind.value().map(|x| (&m.key, x)))
  {
    let key_size = check_write_key_size(key)?;
    total_payload_size += check_value_size(value)? + key_size;
    total_key_size += key_size;
  }

  for enqueue in &enqueues {
    total_payload_size += check_enqueue_payload_size(&enqueue.payload)?;
    if let Some(schedule) = enqueue.backoff_schedule.as_ref() {
      total_payload_size += 4 * schedule.len();
    }
  }

  if total_payload_size > MAX_TOTAL_MUTATION_SIZE_BYTES {
    return Err(type_error(format!(
      "total mutation size too large (max {} bytes)",
      MAX_TOTAL_MUTATION_SIZE_BYTES
    )));
  }

  if total_key_size > MAX_TOTAL_KEY_SIZE_BYTES {
    return Err(type_error(format!(
      "total key size too large (max {} bytes)",
      MAX_TOTAL_KEY_SIZE_BYTES
    )));
  }

  let atomic_write = AtomicWrite {
    checks: transmute_vec(checks),
    mutations,
    enqueues,
  };

  Ok(async move {
    let result = db.atomic_write(atomic_write).await?;

    Ok(result.map(|res| faster_hex::hex_string(&res.versionstamp)))
  })
}

#[op2]
#[string]
fn op_kv_encode_cursor<'a>(
  scope: &mut v8::HandleScope<'a>,
  // (prefix, start, end)
  #[from_v8] TupleArray((OptionNull(prefix), OptionNull(start), OptionNull(end))): TupleArray<(
    OptionNull<V8KvKey>,
    OptionNull<V8KvKey>,
    OptionNull<V8KvKey>,
  )>,
  #[from_v8] boundary_key: V8KvKey,
) -> Result<String, AnyError> {
  let selector = RawSelector::from_tuple(
    prefix.map(cast_keyparts),
    start.map(cast_keyparts),
    end.map(cast_keyparts),
    scope,
  )?;
  let boundary_key = encode_v8_key(cast_keyparts(boundary_key), scope)?;
  let cursor = encode_cursor(&selector, &boundary_key)?;
  Ok(cursor)
}

fn check_read_key_size(key: &[u8]) -> Result<(), AnyError> {
  if key.len() > MAX_READ_KEY_SIZE_BYTES {
    Err(type_error(format!(
      "key too large for read (max {} bytes)",
      MAX_READ_KEY_SIZE_BYTES
    )))
  } else {
    Ok(())
  }
}

fn check_write_key_size(key: &[u8]) -> Result<usize, AnyError> {
  if key.len() > MAX_WRITE_KEY_SIZE_BYTES {
    Err(type_error(format!(
      "key too large for write (max {} bytes)",
      MAX_WRITE_KEY_SIZE_BYTES
    )))
  } else {
    Ok(key.len())
  }
}

fn check_value_size(value: &KvValue) -> Result<usize, AnyError> {
  let payload = match value {
    KvValue::Bytes(x) => x,
    KvValue::V8(x) => x,
    KvValue::U64(_) => return Ok(8),
  };

  if payload.len() > MAX_VALUE_SIZE_BYTES {
    Err(type_error(format!(
      "value too large (max {} bytes)",
      MAX_VALUE_SIZE_BYTES
    )))
  } else {
    Ok(payload.len())
  }
}

fn check_enqueue_payload_size(payload: &[u8]) -> Result<usize, AnyError> {
  if payload.len() > MAX_VALUE_SIZE_BYTES {
    Err(type_error(format!(
      "enqueue payload too large (max {} bytes)",
      MAX_VALUE_SIZE_BYTES
    )))
  } else {
    Ok(payload.len())
  }
}
