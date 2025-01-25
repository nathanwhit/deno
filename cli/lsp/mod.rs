// Copyright 2018-2025 the Deno authors. MIT license.

use std::future::Future;
use std::time::Duration;

use deno_core::error::AnyError;
use deno_core::unsync::spawn;
pub use repl::ReplCompletionItem;
pub use repl::ReplLanguageServer;
use tower_lsp::LspService;
use tower_lsp::Server;

use self::diagnostics::should_send_diagnostic_batch_index_notifications;
use crate::lsp::language_server::LanguageServer;
use crate::util::sync::AsyncFlag;

mod analysis;
mod cache;
mod capabilities;
mod client;
mod code_lens;
mod completions;
mod config;
mod diagnostics;
mod documents;
mod jsr;
pub mod language_server;
mod logging;
mod lsp_custom;
mod npm;
mod parent_process_checker;
mod path_to_regex;
mod performance;
mod refactor;
mod registries;
mod repl;
mod resolver;
mod search;
mod semantic_tokens;
mod testing;
mod text;
mod tsc;
mod urls;

type DenoLspService = tower_lsp::LspService<LanguageServer>;
type LspRequest = tower_lsp::jsonrpc::Request;
struct Loggy {
  inner: DenoLspService,
  tx: tokio::sync::mpsc::UnboundedSender<LspRequest>,
}

impl tower::Service<LspRequest> for Loggy {
  type Response = <DenoLspService as tower::Service<LspRequest>>::Response;
  type Error = <DenoLspService as tower::Service<LspRequest>>::Error;
  type Future = std::pin::Pin<
    Box<
      dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static,
    >,
  >;

  fn poll_ready(
    &mut self,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Result<(), Self::Error>> {
    self.inner.poll_ready(cx)
  }

  fn call(&mut self, req: LspRequest) -> Self::Future {
    let tx = self.tx.clone();
    let r = req.clone();
    let fut = self.inner.call(req);
    let f = async move {
      let _ = tx.send(r);
      fut.await
    };

    Box::pin(f)
  }
}

pub async fn start() -> Result<(), AnyError> {
  let stdin = tokio::io::stdin();
  let stdout = tokio::io::stdout();

  let shutdown_flag = AsyncFlag::default();
  let builder = LspService::build(|client| {
    language_server::LanguageServer::new(
      client::Client::from_tower(client),
      shutdown_flag.clone(),
    )
  })
  .custom_method(
    lsp_custom::PERFORMANCE_REQUEST,
    LanguageServer::performance_request,
  )
  .custom_method(lsp_custom::TASK_REQUEST, LanguageServer::task_definitions)
  .custom_method(testing::TEST_RUN_REQUEST, LanguageServer::test_run_request)
  .custom_method(
    testing::TEST_RUN_CANCEL_REQUEST,
    LanguageServer::test_run_cancel_request,
  )
  .custom_method(
    lsp_custom::VIRTUAL_TEXT_DOCUMENT,
    LanguageServer::virtual_text_document,
  );

  let builder = if should_send_diagnostic_batch_index_notifications() {
    builder.custom_method(
      lsp_custom::LATEST_DIAGNOSTIC_BATCH_INDEX,
      LanguageServer::latest_diagnostic_batch_index_request,
    )
  } else {
    builder
  };

  let (service, socket) = builder.finish();

  let (tx, buf) = tokio::sync::mpsc::unbounded_channel();
  let svc = Loggy { inner: service, tx };
  tokio::spawn({
    let token = shutdown_flag.clone();
    async move {
      let mut buf = buf;
      let mut reqs = Vec::new();
      loop {
        let mut new = false;
        while let Ok(req) = buf.try_recv() {
          reqs.push(req);
          new = true;
        }
        if new {
          tokio::fs::write(
            "./messages.json",
            deno_core::serde_json::to_string(&reqs).unwrap(),
          )
          .await
          .unwrap();
        }
        tokio::select! {
          biased;
          _ = token.wait_raised() => {
            break;
          }
          _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
      }
    }
  });

  // TODO(nayeemrmn): This shutdown flag is a workaround for
  // https://github.com/denoland/deno/issues/20700. Remove when
  // https://github.com/ebkalderon/tower-lsp/issues/399 is fixed.
  // Force end the server 8 seconds after receiving a shutdown request.
  tokio::select! {
    biased;
    _ = Server::new(stdin, stdout, socket).concurrency_level(256).serve(svc) => {}
    _ = spawn(async move {
      shutdown_flag.wait_raised().await;
      tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    }) => {}
  }

  Ok(())
}
