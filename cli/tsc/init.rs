// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use deno_runtime::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use deno_core::error::AnyError;
use deno_runtime::deno_node::SUPPORTED_BUILTIN_NODE_MODULES;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildInfoResponse {
  pub(crate) build_specifier: String,
  pub(crate) libs: Vec<String>,
  pub(crate) node_built_in_module_names: Vec<String>,
}

pub(crate) struct TscInitState {
  pub(crate) op_crate_libs: HashMap<&'static str, PathBuf>,
  pub(crate) build_libs: Vec<&'static str>,
  pub(crate) path_dts: PathBuf,
}

fn deno_webgpu_get_declaration() -> PathBuf {
  let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  manifest_dir
    .join("tsc")
    .join("dts")
    .join("lib.deno_webgpu.d.ts")
}

impl TscInitState {
  pub(crate) fn new(cwd: &Path) -> Self {
    // libs that are being provided by op crates.
    let mut op_crate_libs = HashMap::new();
    op_crate_libs.insert("deno.cache", deno_cache::get_declaration());
    op_crate_libs.insert("deno.console", deno_console::get_declaration());
    op_crate_libs.insert("deno.url", deno_url::get_declaration());
    op_crate_libs.insert("deno.web", deno_web::get_declaration());
    op_crate_libs.insert("deno.fetch", deno_fetch::get_declaration());
    op_crate_libs.insert("deno.webgpu", deno_webgpu_get_declaration());
    op_crate_libs.insert("deno.websocket", deno_websocket::get_declaration());
    op_crate_libs.insert("deno.webstorage", deno_webstorage::get_declaration());
    op_crate_libs.insert("deno.canvas", deno_canvas::get_declaration());
    op_crate_libs.insert("deno.crypto", deno_crypto::get_declaration());
    op_crate_libs.insert(
      "deno.broadcast_channel",
      deno_broadcast_channel::get_declaration(),
    );
    op_crate_libs.insert("deno.net", deno_net::get_declaration());

    // libs that should be loaded into the isolate before snapshotting.
    let libs = vec![
      // Deno custom type libraries
      "deno.window",
      "deno.worker",
      "deno.shared_globals",
      "deno.ns",
      "deno.unstable",
      // Deno built-in type libraries
      "decorators",
      "decorators.legacy",
      "es5",
      "es2015.collection",
      "es2015.core",
      "es2015",
      "es2015.generator",
      "es2015.iterable",
      "es2015.promise",
      "es2015.proxy",
      "es2015.reflect",
      "es2015.symbol",
      "es2015.symbol.wellknown",
      "es2016.array.include",
      "es2016.intl",
      "es2016",
      "es2017",
      "es2017.date",
      "es2017.intl",
      "es2017.object",
      "es2017.sharedmemory",
      "es2017.string",
      "es2017.typedarrays",
      "es2018.asyncgenerator",
      "es2018.asynciterable",
      "es2018",
      "es2018.intl",
      "es2018.promise",
      "es2018.regexp",
      "es2019.array",
      "es2019",
      "es2019.intl",
      "es2019.object",
      "es2019.string",
      "es2019.symbol",
      "es2020.bigint",
      "es2020",
      "es2020.date",
      "es2020.intl",
      "es2020.number",
      "es2020.promise",
      "es2020.sharedmemory",
      "es2020.string",
      "es2020.symbol.wellknown",
      "es2021",
      "es2021.intl",
      "es2021.promise",
      "es2021.string",
      "es2021.weakref",
      "es2022",
      "es2022.array",
      "es2022.error",
      "es2022.intl",
      "es2022.object",
      "es2022.regexp",
      "es2022.sharedmemory",
      "es2022.string",
      "es2023",
      "es2023.array",
      "es2023.collection",
      "esnext",
      "esnext.array",
      "esnext.collection",
      "esnext.decorators",
      "esnext.disposable",
      "esnext.intl",
      "esnext.object",
      "esnext.promise",
    ];

    let path_dts = cwd.join("tsc/dts");
    // ensure we invalidate the build properly.
    for name in libs.iter() {
      println!(
        "cargo:rerun-if-changed={}",
        path_dts.join(format!("lib.{name}.d.ts")).display()
      );
    }

    // create a copy of the vector that includes any op crate libs to be passed
    // to the JavaScript compiler to build into the snapshot
    let mut build_libs = libs.clone();
    for (op_lib, _) in op_crate_libs.iter() {
      build_libs.push(op_lib.to_owned());
    }

    Self {
      op_crate_libs,
      build_libs,
      path_dts,
    }
  }
}

pub(crate) fn build_info(state: &TscInitState) -> BuildInfoResponse {
  let build_specifier = "asset:///bootstrap.ts".to_string();
  let build_libs = state.build_libs.iter().map(|s| s.to_string()).collect();
  let node_built_in_module_names = SUPPORTED_BUILTIN_NODE_MODULES
    .iter()
    .map(|s| s.to_string())
    .collect();
  BuildInfoResponse {
    build_specifier,
    libs: build_libs,
    node_built_in_module_names,
  }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoadResponse {
  pub(crate) data: String,
  pub(crate) version: String,
  pub(crate) script_kind: i32,
}

pub(crate) fn load(
  TscInitState {
    op_crate_libs,
    path_dts,
    ..
  }: &TscInitState,
  load_specifier: &str,
) -> Result<Option<LoadResponse>, AnyError> {
  let re_asset = lazy_regex::regex!(r"asset:/{3}lib\.(\S+)\.d\.ts");
  let build_specifier = "asset:///bootstrap.ts";

  // we need a basic file to send to tsc to warm it up.
  if load_specifier == build_specifier {
    Ok(Some(LoadResponse {
      data: r#"Deno.writeTextFile("hello.txt", "hello deno!");"#.to_string(),
      version: "1".to_string(),
      // this corresponds to `ts.ScriptKind.TypeScript`
      script_kind: 3,
    }))
    // specifiers come across as `asset:///lib.{lib_name}.d.ts` and we need to
    // parse out just the name so we can lookup the asset.
  } else if let Some(caps) = re_asset.captures(load_specifier) {
    if let Some(lib) = caps.get(1).map(|m| m.as_str()) {
      // if it comes from an op crate, we were supplied with the path to the
      // file.
      let path = if let Some(op_crate_lib) = op_crate_libs.get(lib) {
        PathBuf::from(op_crate_lib).canonicalize()?
        // otherwise we will generate the path ourself
      } else {
        path_dts.join(format!("lib.{lib}.d.ts"))
      };
      let data = std::fs::read_to_string(path)?;
      Ok(Some(LoadResponse {
        data,
        version: "1".to_string(),
        // this corresponds to `ts.ScriptKind.TypeScript`
        script_kind: 3,
      }))
    } else {
      Ok(None)
    }
  } else {
    Ok(None)
  }
}
