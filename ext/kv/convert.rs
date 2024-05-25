// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use deno_core::error::AnyError;
use deno_core::error::StdAnyError;
use deno_core::v8;
use deno_core::FromV8;
use deno_core::ToV8;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// A wrapper type for `Option<T>` that (de)serializes `None` as `null`
#[repr(transparent)]
pub struct OptionNull<T>(pub Option<T>);

impl<T> From<Option<T>> for OptionNull<T> {
  fn from(option: Option<T>) -> Self {
    Self(option)
  }
}

impl<T> From<OptionNull<T>> for Option<T> {
  fn from(value: OptionNull<T>) -> Self {
    value.0
  }
}

impl<'a, T> ToV8<'a> for OptionNull<T>
where
  T: ToV8<'a>,
{
  type Error = T::Error;

  fn to_v8(
    self,
    scope: &mut v8::HandleScope<'a>,
  ) -> Result<v8::Local<'a, v8::Value>, Self::Error> {
    match self.0 {
      Some(value) => value.to_v8(scope),
      None => Ok(v8::null(scope).into()),
    }
  }
}

impl<'a, T> FromV8<'a> for OptionNull<T>
where
  T: FromV8<'a>,
{
  type Error = T::Error;

  fn from_v8(
    scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    if value.is_null() {
      Ok(OptionNull(None))
    } else {
      T::from_v8(scope, value).map(|v| OptionNull(Some(v)))
    }
  }
}

pub struct TupleArray<T>(pub T);

impl<T> From<T> for TupleArray<T> {
  fn from(value: T) -> Self {
    Self(value)
  }
}

macro_rules! impl_tuple {
  ($(($($name: ident $(,)?),+)),+ $(,)?) => {
    $(
      impl<'a, $($name),+> ToV8<'a> for TupleArray<($($name,)+)>
      where
        $($name: ToV8<'a>,)+
      {
        type Error = deno_core::error::StdAnyError;

        fn to_v8(
          self,
          scope: &mut v8::HandleScope<'a>,
        ) -> Result<v8::Local<'a, v8::Value>, Self::Error> {
          #[allow(non_snake_case)]
          let ($($name,)+) = self.0;
          let elements = &[$($name.to_v8(scope).map_err(deno_core::error::AnyError::from)?),+];
          Ok(v8::Array::new_with_elements(scope, elements).into())
        }
      }

      impl<'a, $($name),+> FromV8<'a> for TupleArray<($($name,)+)>
      where
        $($name: FromV8<'a>,)+
      {
        type Error = deno_core::error::StdAnyError;

        fn from_v8(
          scope: &mut v8::HandleScope<'a>,
          value: v8::Local<'a, v8::Value>,
        ) -> Result<Self, Self::Error> {
          let array = v8::Local::<v8::Array>::try_from(value)
            .map_err(deno_core::error::AnyError::from)?;
          let mut i = 0;
          #[allow(non_snake_case)]
          let ($($name,)+) = (
            $(
              {
                let element = array.get_index(scope, i).unwrap();
                let res = $name::from_v8(scope, element).map_err(deno_core::error::AnyError::from)?;
                #[allow(unused)]
                {
                  i += 1;
                }
                res
              },
            )+
          );
          Ok(TupleArray(($($name,)+)))
        }
      }
    )+
  };
}

impl_tuple!(
  (A,),
  (A, B),
  (A, B, C),
  (A, B, C, D),
  (A, B, C, D, E),
  (A, B, C, D, E, F),
  (A, B, C, D, E, F, G),
  (A, B, C, D, E, F, G, H),
  (A, B, C, D, E, F, G, H, I),
  (A, B, C, D, E, F, G, H, I, J),
  (A, B, C, D, E, F, G, H, I, J, K),
);

pub fn any_err(e: impl Into<AnyError>) -> AnyError {
  e.into()
}

#[repr(transparent)]
pub struct VecArray<T>(pub Vec<T>);

impl<T> From<Vec<T>> for VecArray<T> {
  fn from(value: Vec<T>) -> Self {
    VecArray(value)
  }
}

impl<'a, T> ToV8<'a> for VecArray<T>
where
  T: ToV8<'a>,
{
  type Error = StdAnyError;

  fn to_v8(
    self,
    scope: &mut v8::HandleScope<'a>,
  ) -> Result<v8::Local<'a, v8::Value>, Self::Error> {
    let mut buf = Vec::with_capacity(self.0.len());
    for item in self.0 {
      buf.push(item.to_v8(scope).map_err(any_err)?);
    }
    Ok(v8::Array::new_with_elements(scope, &buf).into())
  }
}

impl<'a, T> FromV8<'a> for VecArray<T>
where
  T: FromV8<'a>,
{
  type Error = StdAnyError;
  fn from_v8(
    scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    let arr = v8::Local::<v8::Array>::try_from(value).map_err(any_err)?;
    let mut buf = Vec::with_capacity(arr.length() as usize);
    for i in 0..arr.length() {
      let item = arr.get_index(scope, i).unwrap();
      buf.push(T::from_v8(scope, item).map_err(any_err)?);
    }
    Ok(VecArray(buf))
  }
}

/// A type that transparently wraps another type.
/// This trait is unsafe because it is it must g
pub unsafe trait TransparentWrapper<T> {
  fn from_inner(inner: T) -> Self;
  fn into_inner(self) -> T;
}

#[macro_export]
macro_rules! wrapper_struct {
  ($($v: vis struct $wrapper: ident $( <$($generic: tt),+ > )? ($inner: ty);)+) => {

    $(
      #[repr(transparent)]
      $v struct $wrapper $(< $($generic),+ >)? (pub $inner);

      impl $(< $($generic),+ >)? From<$inner> for $wrapper $(< $($generic),+ >)? {
        fn from(inner: $inner) -> Self {
          Self(inner)
        }
      }

      impl $(< $($generic),+ >)? From<$wrapper $(< $($generic),+ >)?> for $inner {
        fn from(wrapper: $wrapper $(< $($generic),+ >)?) -> Self {
          wrapper.0
        }
      }

      // SAFETY: wrapper structs are transparent
      unsafe impl $(< $($generic),+ >)? $crate::convert_util::TransparentWrapper<$inner> for $wrapper $(< $($generic),+ >)? {
        #[inline]
        fn from_inner(inner: $inner) -> Self {
          Self(inner)
        }

        #[inline]
        fn into_inner(self) -> $inner {
          self.0
        }
      }
    )+
  };
}

pub fn transmute_vec<T: TransparentWrapper<U>, U>(v: Vec<T>) -> Vec<U> {
  let mut v = std::mem::ManuallyDrop::new(v);
  // SAFETY: the pointer, length, and capacity are valid because they
  // were created from a Vec<T>.
  // SAFETY: T is a transparent wrapper around U, so their memory layout
  // is identical.
  unsafe {
    Vec::from_raw_parts(v.as_mut_ptr() as *mut U, v.len(), v.capacity())
  }
}

#[repr(transparent)]
pub struct SerdeWrapper<T>(pub T);

unsafe impl<T> TransparentWrapper<T> for SerdeWrapper<T> {
  fn from_inner(inner: T) -> Self {
    Self(inner)
  }

  fn into_inner(self) -> T {
    self.0
  }
}

impl<'a, T> FromV8<'a> for SerdeWrapper<T>
where
  T: for<'de> serde::Deserialize<'de>,
{
  type Error = StdAnyError;

  fn from_v8(
    scope: &mut v8::HandleScope<'a>,
    value: v8::Local<'a, v8::Value>,
  ) -> Result<Self, Self::Error> {
    let value = deno_core::serde_v8::from_v8(scope, value).map_err(any_err)?;
    Ok(Self(value))
  }
}

impl<'a, T> ToV8<'a> for SerdeWrapper<T>
where
  T: serde::Serialize,
{
  type Error = StdAnyError;

  fn to_v8(
    self,
    scope: &mut v8::HandleScope<'a>,
  ) -> Result<v8::Local<'a, v8::Value>, Self::Error> {
    deno_core::serde_v8::to_v8(scope, &self.0)
      .map_err(any_err)
      .map_err(Into::into)
  }
}
