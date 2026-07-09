//! The `otui-lsp` language server binary.
//!
//! Thin transport shell: it speaks LSP 3.17 over stdio (JSON-RPC 2.0) via `tower-lsp` and
//! delegates all language semantics to [`otui_core`]. The lifecycle, document store and
//! byte-offset ↔ position conversion live in the [`otui_lsp_server`] library; `main` only spins
//! up the tokio runtime and serves the [`Backend`](otui_lsp_server::Backend) over stdin/stdout.

use otui_lsp_server::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
