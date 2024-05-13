// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::env;
use std::path::PathBuf;

use deno_core::snapshot::*;
use deno_runtime::*;

mod ts {
  use super::*;
  use deno_core::error::custom_error;
  use deno_core::error::AnyError;
  use deno_core::op2;
  use deno_core::OpState;
  use serde::Serialize;
  use std::io::Write;
  use std::path::Path;
  use std::path::PathBuf;

  mod init {
    include!("tsc/init.rs");
  }

  #[derive(Debug, Serialize)]
  #[serde(rename_all = "camelCase")]
  struct BuildInfoResponse {
    build_specifier: String,
    libs: Vec<String>,
    node_built_in_module_names: Vec<String>,
  }

  #[op2]
  #[serde]
  fn op_build_info(state: &mut OpState) -> init::BuildInfoResponse {
    let init_state = state.borrow::<init::TscInitState>();
    init::build_info(&init_state)
  }

  #[op2(fast)]
  fn op_is_node_file() -> bool {
    false
  }

  #[op2]
  #[string]
  fn op_script_version(
    _state: &mut OpState,
    #[string] _arg: &str,
  ) -> Result<Option<String>, AnyError> {
    Ok(Some("1".to_string()))
  }

  #[op2]
  #[serde]
  // using the same op that is used in `tsc.rs` for loading modules and reading
  // files, but a slightly different implementation at build time.
  fn op_load(
    state: &mut OpState,
    #[string] load_specifier: &str,
  ) -> Result<init::LoadResponse, AnyError> {
    let init_state = state.borrow::<init::TscInitState>();
    init::load(&init_state, load_specifier)?.ok_or_else(|| {
      custom_error(
        "NotFound",
        format!("Cannot find module: {}", load_specifier),
      )
    })
  }

  deno_core::extension!(deno_tsc,
    ops = [op_build_info, op_is_node_file, op_load, op_script_version],
    js = [
      dir "tsc",
      "00_typescript.js",
      "99_main_compiler.js",
    ],
    options = {
      init_state: init::TscInitState,
    },
    state = |state, options| {
      state.put(options.init_state);
    },
  );

  #[cfg(not(feature = "__lsp_runtime_js_sources"))]
  pub fn create_compiler_snapshot(snapshot_path: PathBuf, cwd: &Path) {
    let init_state = init::TscInitState::new(cwd);
    // ensure we invalidate the build properly.
    for (_, path) in init_state.op_crate_libs.iter() {
      println!("cargo:rerun-if-changed={}", path.display());
    }

    // used in the tests to verify that after snapshotting it has the same number
    // of lib files loaded and hasn't included any ones lazily loaded from Rust
    std::fs::write(
      PathBuf::from(env::var_os("OUT_DIR").unwrap())
        .join("lib_file_names.json"),
      serde_json::to_string(&init_state.build_libs).unwrap(),
    )
    .unwrap();

    let output = create_snapshot(
      CreateSnapshotOptions {
        cargo_manifest_dir: env!("CARGO_MANIFEST_DIR"),
        startup_snapshot: None,
        extensions: vec![deno_tsc::init_ops_and_esm(init_state)],
        extension_transpiler: None,
        with_runtime_cb: None,
        skip_op_registration: false,
      },
      None,
    )
    .unwrap();

    // NOTE(bartlomieju): Compressing the TSC snapshot in debug build took
    // ~45s on M1 MacBook Pro; without compression it took ~1s.
    // Thus we're not using compressed snapshot, trading off
    // a lot of build time for some startup time in debug build.
    let mut file = std::fs::File::create(snapshot_path).unwrap();
    if cfg!(debug_assertions) {
      file.write_all(&output.output).unwrap();
    } else {
      let mut vec = Vec::with_capacity(output.output.len());
      vec.extend((output.output.len() as u32).to_le_bytes());
      vec.extend_from_slice(
        &zstd::bulk::compress(&output.output, 22)
          .expect("snapshot compression failed"),
      );
      file.write_all(&vec).unwrap();
    }

    for path in output.files_loaded_during_snapshot {
      println!("cargo:rerun-if-changed={}", path.display());
    }
  }

  pub(crate) fn version() -> String {
    let file_text = std::fs::read_to_string("tsc/00_typescript.js").unwrap();
    let version_text = "  version = \"";
    for line in file_text.lines() {
      if let Some(index) = line.find(version_text) {
        let remaining_line = &line[index + version_text.len()..];
        return remaining_line[..remaining_line.find('"').unwrap()].to_string();
      }
    }
    panic!("Could not find ts version.")
  }
}

#[cfg(not(feature = "__runtime_js_sources"))]
fn create_cli_snapshot(snapshot_path: PathBuf) {
  use deno_runtime::ops::bootstrap::SnapshotOptions;

  // NOTE(bartlomieju): keep in sync with `cli/version.rs`.
  // Ideally we could deduplicate that code.
  fn deno_version() -> String {
    if env::var("DENO_CANARY").is_ok() {
      format!("{}+{}", env!("CARGO_PKG_VERSION"), &git_commit_hash()[..7])
    } else {
      env!("CARGO_PKG_VERSION").to_string()
    }
  }

  let snapshot_options = SnapshotOptions {
    deno_version: deno_version(),
    ts_version: ts::version(),
    v8_version: deno_core::v8_version(),
    target: std::env::var("TARGET").unwrap(),
  };

  deno_runtime::snapshot::create_runtime_snapshot(
    snapshot_path,
    snapshot_options,
    vec![],
  );
}

fn git_commit_hash() -> String {
  if let Ok(output) = std::process::Command::new("git")
    .arg("rev-list")
    .arg("-1")
    .arg("HEAD")
    .output()
  {
    if output.status.success() {
      std::str::from_utf8(&output.stdout[..40])
        .unwrap()
        .to_string()
    } else {
      // When not in git repository
      // (e.g. when the user install by `cargo install deno`)
      "UNKNOWN".to_string()
    }
  } else {
    // When there is no git command for some reason
    "UNKNOWN".to_string()
  }
}

fn main() {
  // Skip building from docs.rs.
  if env::var_os("DOCS_RS").is_some() {
    return;
  }

  // Host snapshots won't work when cross compiling.
  let target = env::var("TARGET").unwrap();
  let host = env::var("HOST").unwrap();
  let skip_cross_check =
    env::var("DENO_SKIP_CROSS_BUILD_CHECK").map_or(false, |v| v == "1");
  if !skip_cross_check && target != host {
    panic!("Cross compiling with snapshot is not supported.");
  }

  let symbols_file_name = match env::consts::OS {
    "android" | "freebsd" | "openbsd" => {
      "generated_symbol_exports_list_linux.def".to_string()
    }
    os => format!("generated_symbol_exports_list_{}.def", os),
  };
  let symbols_path = std::path::Path::new("napi")
    .join(symbols_file_name)
    .canonicalize()
    .expect(
        "Missing symbols list! Generate using tools/napi/generate_symbols_lists.js",
    );

  #[cfg(target_os = "windows")]
  println!(
    "cargo:rustc-link-arg-bin=deno=/DEF:{}",
    symbols_path.display()
  );

  #[cfg(target_os = "macos")]
  println!(
    "cargo:rustc-link-arg-bin=deno=-Wl,-exported_symbols_list,{}",
    symbols_path.display()
  );

  #[cfg(target_os = "linux")]
  {
    // If a custom compiler is set, the glibc version is not reliable.
    // Here, we assume that if a custom compiler is used, that it will be modern enough to support a dynamic symbol list.
    if env::var("CC").is_err()
      && glibc_version::get_version()
        .map(|ver| ver.major <= 2 && ver.minor < 35)
        .unwrap_or(false)
    {
      println!("cargo:warning=Compiling with all symbols exported, this will result in a larger binary. Please use glibc 2.35 or later for an optimised build.");
      println!("cargo:rustc-link-arg-bin=deno=-rdynamic");
    } else {
      println!(
        "cargo:rustc-link-arg-bin=deno=-Wl,--export-dynamic-symbol-list={}",
        symbols_path.display()
      );
    }
  }

  #[cfg(target_os = "android")]
  println!(
    "cargo:rustc-link-arg-bin=deno=-Wl,--export-dynamic-symbol-list={}",
    symbols_path.display()
  );

  // To debug snapshot issues uncomment:
  // op_fetch_asset::trace_serializer();

  if let Ok(c) = env::var("DENO_CANARY") {
    println!("cargo:rustc-env=DENO_CANARY={c}");
  }
  println!("cargo:rerun-if-env-changed=DENO_CANARY");

  println!("cargo:rustc-env=GIT_COMMIT_HASH={}", git_commit_hash());
  println!("cargo:rerun-if-env-changed=GIT_COMMIT_HASH");
  println!(
    "cargo:rustc-env=GIT_COMMIT_HASH_SHORT={}",
    &git_commit_hash()[..7]
  );

  let ts_version = ts::version();
  debug_assert_eq!(ts_version, "5.4.5"); // bump this assertion when it changes
  println!("cargo:rustc-env=TS_VERSION={}", ts_version);
  println!("cargo:rerun-if-env-changed=TS_VERSION");

  println!("cargo:rustc-env=TARGET={}", env::var("TARGET").unwrap());
  println!("cargo:rustc-env=PROFILE={}", env::var("PROFILE").unwrap());

  let c = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
  let o = PathBuf::from(env::var_os("OUT_DIR").unwrap());

  let compiler_snapshot_path = o.join("COMPILER_SNAPSHOT.bin");
  #[cfg(not(feature = "__lsp_runtime_js_sources"))]
  ts::create_compiler_snapshot(compiler_snapshot_path, &c);

  #[cfg(not(feature = "__runtime_js_sources"))]
  {
    let cli_snapshot_path = o.join("CLI_SNAPSHOT.bin");
    create_cli_snapshot(cli_snapshot_path);
  }

  #[cfg(target_os = "windows")]
  {
    let mut res = winres::WindowsResource::new();
    res.set_icon("deno.ico");
    res.set_language(winapi::um::winnt::MAKELANGID(
      winapi::um::winnt::LANG_ENGLISH,
      winapi::um::winnt::SUBLANG_ENGLISH_US,
    ));
    res.compile().unwrap();
  }
}
