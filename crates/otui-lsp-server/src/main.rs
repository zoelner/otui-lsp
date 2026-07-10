//! The `otui-lsp` language server binary.
//!
//! Thin transport shell: it speaks LSP 3.17 over stdio (JSON-RPC 2.0) via the low-level
//! [`lsp_server`] crate and delegates all language semantics to [`otui_core`]. The lifecycle,
//! document store and byte-offset ↔ position conversion live in the [`otui_lsp_server`] library;
//! `main` only performs the initialize handshake and drives a single-threaded, blocking
//! receive loop over stdin/stdout.

use std::error::Error;

use lsp_server::{Connection, Notification};
use lsp_types::InitializeParams;
use otui_lsp_server::{serve, Backend};

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    // The transport: a pair of crossbeam channels wired to stdin/stdout by dedicated I/O threads.
    let (connection, io_threads) = Connection::stdio();

    // Handshake. We need the client's `InitializeParams` before building capabilities (to negotiate
    // position encoding and hierarchical-symbol support and to capture workspace roots), so use the
    // split `initialize_start`/`initialize_finish` form rather than `Connection::initialize`.
    let (initialize_id, initialize_params) = connection.initialize_start()?;
    let initialize_params: InitializeParams = serde_json::from_value(initialize_params)?;

    let backend = Backend::new(connection.sender.clone(), &initialize_params);
    let initialize_result = serde_json::to_value(backend.initialize_result())?;
    connection.initialize_finish(initialize_id, initialize_result)?;

    // `initialize_finish` consumes the client's `initialized` notification, so drive our
    // post-initialization work (the two dynamic registrations + the background workspace scan)
    // through the same notification dispatch, exactly once.
    backend.handle_notification(Notification {
        method: "initialized".to_owned(),
        params: serde_json::Value::Null,
    });

    // Single-threaded main loop: one message at a time (correct and simplest for our low
    // message-rate server). The only offloaded work is the initial workspace scan, spawned onto its
    // own `std::thread` inside the `initialized` handler. `serve` runs until the LSP lifecycle ends
    // and reports how to terminate: a clean `shutdown` → `exit` exits 0, a standalone `exit` (or a
    // dropped connection) exits 1, as the spec requires.
    let termination = serve(&backend, &connection)?;

    io_threads.join()?;
    std::process::exit(termination.exit_code());
}
