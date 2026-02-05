// Copyright 2018-2026 the Deno authors. MIT license.

use std::env;
use std::path::PathBuf;

fn main() {
  let vendor_dir =
    PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("vendor");
  let include_dir = vendor_dir.join("include");
  let src_dir = vendor_dir.join("src");

  println!("cargo:rerun-if-changed=vendor");

  let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
  let target_family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap();

  let mut build = cc::Build::new();
  build
    .include(&include_dir)
    .include(&src_dir)
    .flag_if_supported("-fno-strict-aliasing")
    .flag_if_supported("-Wno-unused-parameter")
    .flag_if_supported("-Wno-gnu-folding-constant");

  // Common sources
  let common_sources = [
    "fs-poll.c",
    "idna.c",
    "inet.c",
    "random.c",
    "strscpy.c",
    "strtok.c",
    "thread-common.c",
    "threadpool.c",
    "timer.c",
    "uv-common.c",
    "uv-data-getter-setters.c",
    "version.c",
  ];

  for source in &common_sources {
    build.file(src_dir.join(source));
  }

  if target_family == "windows" {
    build
      .define("WIN32_LEAN_AND_MEAN", None)
      .define("_WIN32_WINNT", "0x0A00")
      .define("_CRT_DECLARE_NONSTDC_NAMES", "0");

    let win_sources = [
      "win/async.c",
      "win/core.c",
      "win/detect-wakeup.c",
      "win/dl.c",
      "win/error.c",
      "win/fs.c",
      "win/fs-event.c",
      "win/getaddrinfo.c",
      "win/getnameinfo.c",
      "win/handle.c",
      "win/loop-watcher.c",
      "win/pipe.c",
      "win/thread.c",
      "win/poll.c",
      "win/process.c",
      "win/process-stdio.c",
      "win/signal.c",
      "win/snprintf.c",
      "win/stream.c",
      "win/tcp.c",
      "win/tty.c",
      "win/udp.c",
      "win/util.c",
      "win/winapi.c",
      "win/winsock.c",
    ];

    for source in &win_sources {
      build.file(src_dir.join(source));
    }

    // Windows libraries
    println!("cargo:rustc-link-lib=psapi");
    println!("cargo:rustc-link-lib=user32");
    println!("cargo:rustc-link-lib=advapi32");
    println!("cargo:rustc-link-lib=iphlpapi");
    println!("cargo:rustc-link-lib=userenv");
    println!("cargo:rustc-link-lib=ws2_32");
    println!("cargo:rustc-link-lib=dbghelp");
    println!("cargo:rustc-link-lib=ole32");
    println!("cargo:rustc-link-lib=shell32");
  } else {
    // Unix-like systems
    build
      .define("_FILE_OFFSET_BITS", "64")
      .define("_LARGEFILE_SOURCE", None);

    let unix_sources = [
      "unix/async.c",
      "unix/core.c",
      "unix/dl.c",
      "unix/fs.c",
      "unix/getaddrinfo.c",
      "unix/getnameinfo.c",
      "unix/loop-watcher.c",
      "unix/loop.c",
      "unix/pipe.c",
      "unix/poll.c",
      "unix/process.c",
      "unix/random-devurandom.c",
      "unix/signal.c",
      "unix/stream.c",
      "unix/tcp.c",
      "unix/thread.c",
      "unix/tty.c",
      "unix/udp.c",
    ];

    for source in &unix_sources {
      build.file(src_dir.join(source));
    }

    if target_os != "android" {
      println!("cargo:rustc-link-lib=pthread");
    }

    // Platform-specific sources and defines
    match target_os.as_str() {
      "macos" | "ios" | "tvos" | "watchos" => {
        build
          .define("_DARWIN_UNLIMITED_SELECT", "1")
          .define("_DARWIN_USE_64_BIT_INODE", "1");
        build.file(src_dir.join("unix/darwin-proctitle.c"));
        build.file(src_dir.join("unix/darwin.c"));
        build.file(src_dir.join("unix/fsevents.c"));
        build.file(src_dir.join("unix/proctitle.c"));
        build.file(src_dir.join("unix/bsd-ifaddrs.c"));
        build.file(src_dir.join("unix/kqueue.c"));
        build.file(src_dir.join("unix/random-getentropy.c"));
        println!("cargo:rustc-link-lib=m");
      }
      "linux" => {
        build
          .define("_GNU_SOURCE", None)
          .define("_POSIX_C_SOURCE", "200112");
        build.file(src_dir.join("unix/linux.c"));
        build.file(src_dir.join("unix/procfs-exepath.c"));
        build.file(src_dir.join("unix/proctitle.c"));
        build.file(src_dir.join("unix/random-getrandom.c"));
        build.file(src_dir.join("unix/random-sysctl-linux.c"));
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=rt");
        println!("cargo:rustc-link-lib=m");
      }
      "android" => {
        build.define("_GNU_SOURCE", None);
        build.file(src_dir.join("unix/linux.c"));
        build.file(src_dir.join("unix/procfs-exepath.c"));
        build.file(src_dir.join("unix/proctitle.c"));
        build.file(src_dir.join("unix/random-getentropy.c"));
        build.file(src_dir.join("unix/random-getrandom.c"));
        build.file(src_dir.join("unix/random-sysctl-linux.c"));
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=m");
      }
      "freebsd" => {
        build.file(src_dir.join("unix/freebsd.c"));
        build.file(src_dir.join("unix/posix-hrtime.c"));
        build.file(src_dir.join("unix/bsd-proctitle.c"));
        build.file(src_dir.join("unix/bsd-ifaddrs.c"));
        build.file(src_dir.join("unix/kqueue.c"));
        build.file(src_dir.join("unix/random-getrandom.c"));
        println!("cargo:rustc-link-lib=m");
      }
      "dragonfly" => {
        build.file(src_dir.join("unix/freebsd.c"));
        build.file(src_dir.join("unix/posix-hrtime.c"));
        build.file(src_dir.join("unix/bsd-proctitle.c"));
        build.file(src_dir.join("unix/bsd-ifaddrs.c"));
        build.file(src_dir.join("unix/kqueue.c"));
        println!("cargo:rustc-link-lib=m");
      }
      "openbsd" => {
        build.file(src_dir.join("unix/openbsd.c"));
        build.file(src_dir.join("unix/posix-hrtime.c"));
        build.file(src_dir.join("unix/bsd-proctitle.c"));
        build.file(src_dir.join("unix/bsd-ifaddrs.c"));
        build.file(src_dir.join("unix/kqueue.c"));
        build.file(src_dir.join("unix/random-getentropy.c"));
        println!("cargo:rustc-link-lib=m");
      }
      "netbsd" => {
        build.file(src_dir.join("unix/netbsd.c"));
        build.file(src_dir.join("unix/posix-hrtime.c"));
        build.file(src_dir.join("unix/bsd-proctitle.c"));
        build.file(src_dir.join("unix/bsd-ifaddrs.c"));
        build.file(src_dir.join("unix/kqueue.c"));
        println!("cargo:rustc-link-lib=kvm");
        println!("cargo:rustc-link-lib=m");
      }
      "solaris" | "illumos" => {
        build.define("__EXTENSIONS__", None);
        build.define("_XOPEN_SOURCE", "500");
        build.define("_REENTRANT", None);
        build.file(src_dir.join("unix/no-proctitle.c"));
        build.file(src_dir.join("unix/sunos.c"));
        println!("cargo:rustc-link-lib=kstat");
        println!("cargo:rustc-link-lib=nsl");
        println!("cargo:rustc-link-lib=sendfile");
        println!("cargo:rustc-link-lib=socket");
      }
      _ => {}
    }
  }

  build.compile("uv");

  // Generate bindings
  let bindings = bindgen::Builder::default()
    .header(include_dir.join("uv.h").to_string_lossy())
    .clang_arg(format!("-I{}", include_dir.display()))
    .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
    .allowlist_function("uv_.*")
    .allowlist_type("uv_.*")
    .allowlist_var("UV_.*")
    .generate()
    .expect("Unable to generate bindings");

  let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
  bindings
    .write_to_file(out_path.join("bindings.rs"))
    .expect("Couldn't write bindings!");
}
