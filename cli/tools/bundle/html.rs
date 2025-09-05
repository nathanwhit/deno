// Copyright 2018-2025 the Deno authors. MIT license.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use capacity_builder::StringBuilder;
use deno_core::anyhow;
use deno_core::error::AnyError;
use lol_html::element;
use lol_html::html_content::ContentType as LolContentType;

use crate::tools::bundle::OutputFile;

#[derive(Debug, Clone)]
pub struct Script {
  pub src: Option<String>,
  pub is_async: bool,
  pub is_module: bool,
  pub resolved_path: Option<PathBuf>,
}

struct Attr<'a> {
  name: Cow<'static, str>,
  value: Option<Cow<'a, str>>,
}

impl<'a> Attr<'a> {
  fn new(
    name: impl Into<Cow<'static, str>>,
    value: Option<Cow<'a, str>>,
  ) -> Self {
    Self {
      name: name.into(),
      value,
    }
  }
  fn write_out<'s>(&'s self, out: &mut StringBuilder<'s>)
  where
    'a: 's,
  {
    out.append(&self.name);
    if let Some(value) = &self.value {
      out.append("=\"");
      out.append(value);
      out.append('"');
    }
  }
}

fn write_attr_list<'a, 's>(attrs: &'s [Attr<'a>], out: &mut StringBuilder<'s>)
where
  'a: 's,
{
  if attrs.is_empty() {
    return;
  }

  out.append(' ');
  for i in 0..attrs.len() - 1 {
    attrs[i].write_out(out);
    out.append(' ');
  }

  attrs[attrs.len() - 1].write_out(out);
}

impl Script {
  pub fn to_element_string(&self) -> String {
    let mut attrs = Vec::new();
    if let Some(src) = &self.src {
      attrs.push(Attr::new("src", Some(Cow::Borrowed(src))));
    }
    if self.is_async {
      attrs.push(Attr::new("async", None));
    }
    if self.is_module {
      attrs.push(Attr::new("type", Some("module".into())));
    }
    attrs.push(Attr::new("crossorigin", None));
    StringBuilder::build(|out| {
      out.append("<script");

      write_attr_list(&attrs, out);

      out.append("></script>");
    })
    .unwrap()
  }
}

struct NoOutput;

impl lol_html::OutputSink for NoOutput {
  fn handle_chunk(&mut self, _: &[u8]) {}
}

fn collect_scripts(doc: &str) -> Result<Vec<Script>, AnyError> {
  let mut scripts = Vec::new();
  let mut rewriter = lol_html::HtmlRewriter::new(
    lol_html::Settings {
      element_content_handlers: vec![element!("script[src]", |el| {
        let is_ignored =
          el.has_attribute("deno-ignore") || el.has_attribute("vite-ignore");
        if is_ignored {
          return Ok(());
        }
        let typ = el.get_attribute("type");
        let (Some("module") | None) = typ.as_deref() else {
          return Ok(());
        };
        let src = el.get_attribute("src").unwrap();
        let is_async = el.has_attribute("async");
        let is_module = matches!(typ.as_deref(), Some("module"));

        scripts.push(Script {
          src: Some(src),
          is_async,
          is_module,
          resolved_path: None,
        });
        Ok(())
      })],
      ..lol_html::Settings::new()
    },
    NoOutput,
  );
  rewriter.write(doc.as_bytes())?;
  rewriter.end()?;
  Ok(scripts)
}

#[derive(Debug, Clone)]
pub struct HtmlEntrypoint {
  pub path: PathBuf,
  pub scripts: Vec<Script>,
  pub temp_module: String,
  pub contents: String,
  pub entry_name: String,
}

// Helper to create a filesystem-friendly name based on a path
fn sanitize_entry_name(cwd: &Path, path: &Path) -> String {
  let rel =
    pathdiff::diff_paths(path, cwd).unwrap_or_else(|| path.to_path_buf());
  let stem = rel
    .with_extension("")
    .to_string_lossy()
    .replace(['\\', '/'], "$PATHSEP$");
  if stem.is_empty() {
    "entry".to_string()
  } else {
    stem
  }
}

fn desanitize_entry_name(name: &str) -> String {
  name.replace("$PATHSEP$", std::path::MAIN_SEPARATOR_STR)
}

fn parse_html_entrypoint(
  cwd: &Path,
  path: &Path,
  contents: String,
) -> anyhow::Result<HtmlEntrypoint> {
  let mut scripts = collect_scripts(&contents)?;

  let mut temp_module = String::new();
  for script in &mut scripts {
    if let Some(src) = &mut script.src {
      let src = src.trim_start_matches('/');
      let path = path.parent().unwrap().join(src);

      temp_module
        .push_str(&format!("import \"{}\";\n", path.to_string_lossy()));
      script.resolved_path = Some(path);
    }
  }

  Ok(HtmlEntrypoint {
    path: path.to_path_buf(),
    scripts,
    temp_module,
    contents,
    entry_name: sanitize_entry_name(cwd, path),
  })
}

pub fn load_html_entrypoint(
  cwd: &Path,
  path: &Path,
) -> anyhow::Result<HtmlEntrypoint> {
  let contents = std::fs::read_to_string(path)?;
  parse_html_entrypoint(cwd, path, contents)
}

pub struct HtmlOutputFiles<'a, 'f> {
  output_files: &'f mut Vec<OutputFile<'a>>,
  index: HashMap<String, PathBuf>,
}

impl<'a, 'f> HtmlOutputFiles<'a, 'f> {
  pub fn new(output_files: &'f mut Vec<OutputFile<'a>>) -> Self {
    let mut index = std::collections::HashMap::new();
    for f in output_files.iter() {
      if let Some(name) = f.path.file_name().and_then(|s| s.to_str()) {
        index.insert(name.to_string(), f.path.clone());
      }
    }
    Self {
      output_files,
      index,
    }
  }
}

impl HtmlEntrypoint {
  pub fn patch_html_with_response<'a>(
    self,
    cwd: &Path,
    outdir: &Path,
    html_output_files: &mut HtmlOutputFiles<'a, '_>,
  ) -> anyhow::Result<()> {
    eprintln!("outdir: {:?}; self.path: {:?}", outdir, self.path);
    let html_out_path = {
      outdir.join(&format!("{}.html", desanitize_entry_name(&self.entry_name)))
    };
    eprintln!("html_out_path: {:?}", html_out_path);

    if self.scripts.is_empty() {
      html_output_files.output_files.push(OutputFile {
        path: html_out_path,
        contents: Cow::Owned(self.contents.into_bytes()),
        hash: None,
      });
      return Ok(());
    }

    // With hashed patterns enabled, the output names will be
    //   <entry>-<hash>.js and optionally <entry>-<hash>.css
    // Fallback to non-hashed names if patterns are not applied.
    let js_out = html_output_files
      .index
      .get(&format!("{}.js", self.entry_name))
      .cloned();
    let css_out = html_output_files
      .index
      .get(&format!("{}.css", self.entry_name))
      .cloned();

    eprintln!("js_out: {:?}", js_out);
    eprintln!("css_out: {:?}", css_out);

    let script_src = js_out.as_ref().map(|p| {
      let base = html_out_path.parent().unwrap_or(outdir);
      let mut rel = pathdiff::diff_paths(p, base)
        .unwrap_or_else(|| p.clone())
        .to_string_lossy()
        .to_string();
      if std::path::MAIN_SEPARATOR != '/' {
        rel = rel.replace('\\', "/");
      }
      rel
    });
    eprintln!("script_src: {:?}", script_src);
    let any_async = self.scripts.iter().any(|s| s.is_async);
    let any_module = self.scripts.iter().any(|s| s.is_module);

    if let Some(script_src) = script_src {
      let to_inject = Script {
        src: Some(
          if !script_src.starts_with(".") && !script_src.starts_with("/") {
            format!("./{}", script_src)
          } else {
            script_src
          },
        ),
        is_async: any_async,
        is_module: any_module,
        resolved_path: None,
      };

      let css_href = css_out.as_ref().map(|p| {
        let base = html_out_path.parent().unwrap_or(outdir);
        let mut rel = pathdiff::diff_paths(p, base)
          .unwrap_or_else(|| p.clone())
          .to_string_lossy()
          .to_string();
        if std::path::MAIN_SEPARATOR != '/' {
          rel = rel.replace('\\', "/");
        }
        rel
      });

      let patched = inject_scripts_and_css(
        &self.contents,
        to_inject,
        &self.scripts,
        css_href,
      )?;

      html_output_files.output_files.push(OutputFile {
        path: html_out_path,
        contents: Cow::Owned(patched.into_bytes()),
        hash: None,
      });
    } else {
      // Missing JS output for the page's entry
      return Err(deno_core::anyhow::anyhow!(
        "failed to locate output for HTML entry '{}'",
        self.entry_name
      ));
    }

    Ok(())
  }
}

fn make_link_str(attrs: &[Attr]) -> String {
  StringBuilder::build(|out| {
    out.append("<link");
    write_attr_list(attrs, out);
    out.append(">");
  })
  .unwrap()
}

fn stylesheet_str(path: &str) -> String {
  let attrs = &[
    Attr::new("rel", Some("stylesheet".into())),
    Attr::new("crossorigin", None),
    Attr::new("href", Some(Cow::Borrowed(path))),
  ];
  make_link_str(attrs)
}

fn inject_scripts_and_css(
  input: &str,
  to_inject: Script,
  to_remove: &[Script],
  css_to_inject_path: Option<String>,
) -> anyhow::Result<String> {
  let did_inject = Cell::new(false);
  let rewritten = lol_html::rewrite_str(
    input,
    lol_html::Settings {
      element_content_handlers: vec![
        element!("head", |el| {
          let already_done = did_inject.replace(true);
          if already_done {
            return Ok(());
          }
          el.append(&to_inject.to_element_string(), LolContentType::Html);

          if let Some(css_to_inject_path) = &css_to_inject_path {
            let link = stylesheet_str(css_to_inject_path);
            el.append(&link, LolContentType::Html);
          }

          Ok(())
        }),
        element!("script[src]", |el| {
          let src = el.get_attribute("src").unwrap();
          if to_remove
            .iter()
            .any(|script| script.src.as_deref() == Some(src.as_str()))
          {
            el.remove();
          }
          Ok(())
        }),
      ],
      document_content_handlers: vec![lol_html::end!(|end| {
        if !did_inject.replace(true) {
          let script = to_inject.to_element_string();
          let link = css_to_inject_path
            .as_ref()
            .map(|p| stylesheet_str(p))
            .unwrap_or_default();
          end.append(
            &format!("<head>{script}{link}</head>"),
            LolContentType::Html,
          );
        }
        Ok(())
      })],
      ..lol_html::Settings::new()
    },
  )?;
  Ok(rewritten)
}

fn reencode_hash(hash: &str) -> String {
  base32::encode(
    base32::Alphabet::Rfc4648 { padding: false },
    hash.as_bytes(),
  )
}
