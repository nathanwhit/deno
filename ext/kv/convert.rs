// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

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
