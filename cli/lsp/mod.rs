// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use deno_core::error::AnyError;
use deno_core::unsync::spawn;
use tokio_util::sync::CancellationToken;
use tower_lsp::LspService;
use tower_lsp::Server;

use crate::lsp::language_server::LanguageServer;
pub use repl::ReplCompletionItem;
pub use repl::ReplLanguageServer;

use self::diagnostics::should_send_diagnostic_batch_index_notifications;

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
mod search;
mod semantic_tokens;
mod testing;
mod text;
mod tsc;
mod urls;

struct Loggy {
  inner: tower_lsp::LspService<LanguageServer>,
  tx: tokio::sync::mpsc::UnboundedSender<tower_lsp::jsonrpc::Request>,
}

impl tower::Service<tower_lsp::jsonrpc::Request> for Loggy {
  type Response = <tower_lsp::LspService<LanguageServer> as tower::Service<
    tower_lsp::jsonrpc::Request,
  >>::Response;
  type Error = <tower_lsp::LspService<LanguageServer> as tower::Service<
    tower_lsp::jsonrpc::Request,
  >>::Error;
  type Future = std::pin::Pin<
    Box<
      dyn std::future::Future<Output = Result<Self::Response, Self::Error>>
        + Send
        + 'static,
    >,
  >;

  fn poll_ready(
    &mut self,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Result<(), Self::Error>> {
    self.inner.poll_ready(cx)
  }

  fn call(&mut self, req: tower_lsp::jsonrpc::Request) -> Self::Future {
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

  let token = CancellationToken::new();
  let builder = LspService::build(|client| {
    language_server::LanguageServer::new(
      client::Client::from_tower(client),
      token.clone(),
    )
  })
  .custom_method(
    lsp_custom::PERFORMANCE_REQUEST,
    LanguageServer::performance_request,
  )
  .custom_method(lsp_custom::TASK_REQUEST, LanguageServer::task_definitions)
  // TODO(nayeemrmn): Rename this to `deno/taskDefinitions` in vscode_deno and
  // remove this alias.
  .custom_method("deno/task", LanguageServer::task_definitions)
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

  // let (tx, buf) =
  //   tokio::sync::mpsc::unbounded_channel::<tower_lsp::jsonrpc::Request>();

  // let svc = Loggy { inner: service, tx };
  // tokio::spawn({
  //   let token = token.clone();
  //   async move {
  //     let mut buf = buf;
  //     let mut reqs = Vec::new();
  //     loop {
  //       let mut new = false;
  //       while let Ok(req) = buf.try_recv() {
  //         reqs.push(req);
  //         new = true;
  //       }
  //       if new {
  //         tokio::fs::write(
  //           "./messages.json",
  //           deno_core::serde_json::to_string(&reqs).unwrap(),
  //         )
  //         .await
  //         .unwrap();
  //       }
  //       tokio::select! {
  //         biased;
  //         _ = token.cancelled() => {
  //           break;
  //         }
  //         _ = tokio::time::sleep(Duration::from_secs(5)) => {}
  //       }
  //     }
  //   }
  // });

  // TODO(nayeemrmn): This cancellation token is a workaround for
  // https://github.com/denoland/deno/issues/20700. Remove when
  // https://github.com/ebkalderon/tower-lsp/issues/399 is fixed.
  // Force end the server 8 seconds after receiving a shutdown request.
  tokio::select! {
    biased;
    _ = Server::new(stdin, stdout, socket).serve(service) => {}
    // _ = Server::new(stdin, stdout, socket).serve(svc) => {}
    _ = spawn(async move {
      token.cancelled().await;
      tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    }) => {}
  }

  Ok(())
}
