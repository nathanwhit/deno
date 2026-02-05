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
