# deno bundle dev server – design & phases

Author: deno CLI team
Last updated: 2025-09-09

## Goals

- Add a development server to `deno bundle` that serves static files and the bundled output from memory.
- Support two primary modes:
  - `deno bundle --serve <entry>` – serve a one-shot build (no watching).
  - `deno bundle --dev <entry>` – serve and watch; progressively add live reload → CSS hot patching → full HMR.
- Reuse the existing bundling pipeline in `cli/tools/bundle/{mod,html}.rs` and esbuild “context” mode for incremental builds.
- Use `hyper` for HTTP and `fastwebsockets` for WS control channel.
- Avoid writing build outputs to disk in serve/dev mode; keep artifacts in memory.

Non‑goals (initially):
- Reverse proxy, HTTPS termination, or middleware system.
- Server-side rendering or route handlers.
- Support for multiple concurrent projects per process (serve one workspace per invocation).

## CLI UX

- New flags on `deno bundle`:
  - `--serve[=<host:port>]` – start the dev server after building. Default `localhost:5173` when value omitted.
  - `--dev[=<host:port>]` – shorthand for `--serve[=<…>] --watch` and enabling dev client injection. Defaults to same address behavior as `--serve`.
  - `--open` – open default browser to the served URL (optional, can come later).
  - `--no-hmr` – disable HMR features (when implemented); live reload still allowed.
  - `--hmr` (future) – explicitly enable HMR when not using `--dev`.
  - `--port <n>`, `--host <s>` (optional alternatives to the `--serve=<addr>` form).

Rules and validation:
- Positional `file`/`entrypoints` remain required (ts/js/html). `index.html` is accepted.
- When `--serve` or `--dev` is set:
  - `--output`/`--outdir` must not be specified (we serve from memory).
  - HTML entrypoints do not require `--outdir` (current behavior requires it); we will relax this only for serve/dev flows.
- `--dev` implies `--watch`. If both are present, `--dev` wins for any duplicate settings.
- Defaults:
  - `platform=browser` for HTML entrypoints remains enforced by existing logic; for script entries we keep current defaults but set dev-serving behavior accordingly.
  - No caching headers; always disable HTTP caching in dev.

Help text snippets:
- `deno bundle --serve index.html` – builds once and serves from memory at http://localhost:5173.
- `deno bundle --dev index.html` – starts a dev server with watch and live reload.

## High-level Architecture

Key pieces (new in `cli/tools/bundle/` unless stated):
- `DevServer` (new): boots a `hyper` HTTP server and a WS endpoint using `fastwebsockets`.
- `DevAssetStore` (new): in-memory filesystem of build outputs with MIME metadata, ETags, and stable request paths.
- `DevRouter` (new): HTTP routing layer (plain service function) that serves:
  - Static disk files (project root) for non-bundled assets.
  - In-memory outputs for JS/CSS/assets produced by esbuild.
  - HTML entrypoints rewritten to point at in-memory assets and inject the dev client.
  - `GET /__hmr` websocket upgrade for live reload/CSS/HMR.
- `DevClient` (new tiny JS module, emitted inline or served at `/__hmr/client.js`): connects via WS and handles reload/CSS patch/HMR.
- Reused: `EsbuildBundler` in `mod.rs` (context/watch mode) and `html.rs` rewriting utilities.

Concurrency and async:
- Use `tokio` for all async. No blocking lock methods (see Async Guidelines). heavy synchronous work (HTML rewrite, large file IO) goes through `spawn_blocking` as needed.
- Esbuild context mode: seed the in-memory store only after the first real `onEnd` callback (initial build request is a placeholder).

## Integration Points and Refactors (small, surgical)

1) Extend CLI flags (`cli/args/flags.rs`)
- `bundle_subcommand()` – add flags described above.
- `bundle_parse()` – populate new fields on `BundleFlags`:
  - `serve: Option<SocketAddr>` and `dev: Option<SocketAddr>` (or a single `serve: Option<ServeMode>` with `Serve | Dev` and resolved `addr`).
  - `hmr: Option<bool>` (default: when `--dev`, true; otherwise false).
  - Optionally `open: bool`.

2) Pass-through (`BundleFlags` → bundler)
- `BundleFlags` carries the new options into `cli/tools/bundle/mod.rs::bundle()`.
- Adjust `configure_esbuild_flags()` only as needed:
  - In dev/serve with HTML entries, keep current `splitting=true`, `entry_names/chunk_names/asset_names="[dir]/[name]-[hash]"`.
  - For script entries in dev: prefer `format=esm` and `splitting=true` to lay groundwork for HMR (Phase 3) while remaining compatible with existing paths.

3) Output collection to memory
- Introduce `collect_output_files_to_memory()` that reuses `collect_output_files()` logic but does not require an actual disk outdir for HTML entries.
  - New enum `OutTarget`: `Disk { outdir: PathBuf } | Memory { base: RequestBase }`.
  - For `Memory`, compute “virtual” output paths rooted at `RequestBase` (eg. `/@out/…`) and patch HTML via `html::HtmlOutputFiles` with that base.
- Keep existing `maybe_process_contents()` (require-shim replacement) and reuse it before populating memory assets.

4) Dev server module
- File: `cli/tools/bundle/dev_server.rs` (new).
- Types:
  - `DevServerConfig { addr, hmr: bool, css_hot: bool, live_reload: bool, open: bool }`.
  - `DevAsset { path: String, content_type: String, bytes: Bytes, etag: String }`.
  - `DevAssetStore { routes: HashMap<String, DevAsset>, by_disk: HashMap<PathBuf, String> }`.
  - `ServerEvent` (enum): `Reload`, `CssUpdate { urls: Vec<String> }`, `HmrUpdate { updates: Vec<ModuleUpdate> }`, `Errors { diagnostics }`.
  - `Broadcaster` using `tokio::sync::broadcast::{Sender, Receiver}`.
- HTTP routes:
  - `/` or any requested HTML entry file path → serve rewritten HTML (from memory) with dev client injected.
  - `/@out/*` → in-memory compiled assets (JS/CSS/assets). Cache-Control: `no-store`.
  - Any other path → serve from disk (relative to cwd) when exists; otherwise 404.
  - `/__hmr` → websocket upgrade via `fastwebsockets`.
- WS protocol (JSON messages):
  - `{type:"connected", protocol: 1}` on connect.
  - `{type:"reload"}` for full page reload.
  - `{type:"style-update", urls:["/path.css?v=hash", …]}` for CSS hot patching.
  - `{type:"hmr", updates:[{id, url, hash}]}` for module HMR (Phase 3).
  - `{type:"errors", diagnostics:[…]}` for build errors (optional overlay later).

5) Dev client module
- Minimal inline script injected into HTML in dev modes:
  - Connects to `ws(s)://<host>/__hmr`.
  - On `reload` → `location.reload()`.
  - On `style-update` → iterate `<link rel=stylesheet>` and `<style data-href>` to patch URLs by appending `?v=<ts/hash>`.
  - Phase 3: expose `import.meta.hot` shim and module update logic.
- Injection point: extend `html::inject_scripts_and_css()` to optionally inject the dev client after existing script/css injection. Keep this logic in `html.rs` to avoid duplicating rewriting.

6) Orchestration (`bundle()` flow)
- New branch inside `cli/tools/bundle/mod.rs::bundle()`:
  - When `bundle_flags.dev || bundle_flags.serve.is_some()` call `bundle_dev(flags, bundle_flags).await` and return without writing to disk.
- `bundle_dev()` responsibilities:
  - Create `EsbuildBundler` via `bundle_init()`.
  - Do initial `build().await?` then seed memory assets via `collect_output_files_to_memory()`.
  - Start `DevServer` with the seeded `DevAssetStore`.
  - If `--dev` (watch):
    - Reuse `watch_recv(...)` logic to listen for file changes (as `bundle_watch()` does).
    - On rebuild completion, update `DevAssetStore` and broadcast `Reload` or `CssUpdate` events.
  - Keep running until Ctrl+C.

## Serving from Memory

- Request path conventions:
  - All esbuild outputs map under `/@out/<relative-path-from-cwd-or-entry>`; HTML rewrites point there.
  - Keep hashed filenames that esbuild emits; use query params only for CSS hot patching.
- `DevAssetStore` maintains a map from request path to `DevAsset`. Updates replace `bytes` and `etag` atomically under a write lock.
- MIME detection via `mime_guess` (JS, CSS, images, fonts, maps). Always `Cache-Control: no-store`.

## Phased Delivery

Phase 0 – CLI + Serve (no watch)
- Add flags, parse, and plumbing (`BundleFlags`).
- Implement `DevAssetStore`, `DevServer`, route wiring, HTML rewrite + dev client injection gated behind `--dev`.
- Produce build once; serve from memory.
- Acceptance:
  - `deno bundle --serve index.html` serves rewritten HTML and JS/CSS from memory.
  - Static assets referenced by HTML are served from disk.
  - No file system writes when `--serve/--dev` are used.

Phase 1 – Live Reload (watch + full page reload)
- Reuse esbuild context mode that already exists in `EsbuildBundler` and the watch machinery from `bundle_watch()`.
- On rebuild success:
  - Update `DevAssetStore` with new outputs.
  - Broadcast `{type:"reload"}` to all WS clients.
- On rebuild errors:
  - Log to terminal; optionally broadcast `{type:"errors"}`.
- Acceptance:
  - `deno bundle --dev index.html` reloads the page on any source change and shows the updated bundle.

Phase 2 – CSS Hot Patching
- Compare esbuild metafiles between builds to detect changed CSS outputs.
- If only CSS changed, update `DevAssetStore` and broadcast `{type:"style-update", urls:[…]}`; client swaps hrefs without reloading.
- Ensure HTML rewrite always uses stable request paths for CSS (eg. `/@out/<name>.css`) so updates do not require rewriting HTML.
- Acceptance:
  - Editing CSS updates styles without a full page reload.

Phase 3 – HMR (ESM graph updates)
- Build strategy in dev:
  - Force `format=esm` and `splitting=true` for script entries.
  - Keep `platform=browser`; use esbuild `metafile` to maintain module→chunk mapping.
- Server:
  - On rebuild, diff metafiles and categorize changed JS chunks.
  - Broadcast `{type:"hmr", updates:[{id, url, hash}]}`.
- Client HMR runtime:
  - Provide `import.meta.hot` shim with `accept()` / `dispose()`.
  - Load updated modules by re-importing `url + ?v=<hash>` and drive acceptance graph; on refusal, fall back to full `reload`.
- Limitations:
  - HMR is best-effort; modules without explicit accept cause a full reload.
  - Node polyfills and `cjs` may not support granular HMR; fallback to reload.

## Reuse of Existing Code

- `EsbuildBundler` (context mode) and `bundle_watch()` patterns: copy the watcher plumbing and error handling; avoid duplicating resolution logic.
- `collect_output_files()` and `maybe_process_contents()` to keep processing identical across disk and memory outputs.
- `html.rs` for script/css injection and HTML rewriting; extend with a “serve base” so we can generate request paths for in-memory outputs.

## Testing Strategy

- Unit tests for:
  - HTML rewriting with dev client injection (`cli/tools/bundle/html.rs`).
  - Output collection to memory (new helper).
  - Path mapping and MIME type resolution.
- Integration tests (tokio):
  - Boot server, GET HTML/JS/CSS, assert contents and headers.
  - WS connection, simulate broadcast, and assert client receives messages (server-side only).
  - Watch scenario: change a temp file, trigger rebuild, assert broadcast.

## Telemetry, Logging, DX

- Print: server address, entrypoint path, and build time (reuse `print_finished_message` style but adapted for dev).
- Optional `--open` to launch default browser.
- Consider a `--quiet` mode to reduce noise.

## Error Handling & Cancellation

- Graceful shutdown on Ctrl+C; close WS clients and stop hyper server.
- On esbuild exit, log and terminate server.
- Avoid panics from `blocking_*` locks; use async locks (`RwLock`, `Mutex`) and `spawn_blocking` for heavy sync tasks.

## Implementation Notes

- Server binding uses `hyper` HTTP/1.1 via `hyper_util::rt::TokioIo` similar to `runtime/inspector_server.rs` for WS upgrades.
- WS fanout via `tokio::sync::broadcast` (bounded); each client tasks to forward messages and handle close.
- All responses set `Cache-Control: no-store`; add `ETag` and `Last-Modified` from the `DevAsset` to support dev tooling.
- Respect import maps, npm resolution, and externals: unchanged; all handled by existing resolver + plugin handler.

## Open Questions

- Should `--serve` without an explicit HTML entry provide a generated HTML wrapper for script entries? (MVP: no; require an HTML file.)
- How to expose the dev client script (inline vs `/__hmr/client.js`)? (Start with inline for simplicity.)
- HMR module IDs: use chunk paths from esbuild or stable virtual IDs? (Prefer chunk paths; revise if instability hurts updates.)

## Rollout Plan

- Land Phase 0 behind the new flags (no HMR) and iterate.
- Ship Phase 1 shortly after; Phase 2/3 can be guarded by `--hmr` until stabilized.

