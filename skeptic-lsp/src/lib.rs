//! Language server that runs `cargo test` on save and reports compiler errors
//! and test panics (`panic!`, `assert!`, ...) as editor diagnostics.

// A language server must not take the editor's session down: no panicking shortcuts.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use tower_lsp::{LspService, Server};

mod cargo_output;
mod runner;
mod server;

/// Serves the language server over stdin/stdout until the client disconnects.
pub async fn run_stdio() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(server::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
