use std::path::PathBuf;

fn glob_files(s: &str) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    glob::glob(s)
        .unwrap()
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let glob = glob_files("nghttp2-1.66.0/lib/*.c")?;
    cc::Build::new()
        .files(glob)
        .include("./nghttp2-1.66.0/lib/includes")
        .compile("nghttp2");

    let bindings = bindgen::builder()
        .header("nghttp2-1.66.0/lib/includes/nghttp2/nghttp2.h")
        .clang_arg("-I./nghttp2-1.66.0/lib/includes")
        .allowlist_item("nghttp2_.*")
        .generate()?;

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(std::env::var("OUT_DIR")?);
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");

    Ok(())
}
