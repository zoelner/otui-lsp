//! The `otui-lsp` binary.
//!
//! Run with **no arguments** (how every editor launches it), it is a thin LSP 3.17 transport
//! shell: it speaks the protocol over stdio (JSON-RPC 2.0) via the low-level [`lsp_server`] crate
//! and delegates all language semantics to [`otui_core`]. The lifecycle, document store and
//! byte-offset ↔ position conversion live in the [`otui_lsp_server`] library; `main` only performs
//! the initialize handshake and drives a single-threaded, blocking receive loop over
//! stdin/stdout.
//!
//! It also carries a small **CLI**, rust-analyzer-style, dispatched on `argv[1]` before any of
//! that: `otui-lsp fmt <paths...> [--check|--write]` runs [`otui_lsp_server::cli::run_fmt`], and
//! `otui-lsp check <paths...> [--deny <level>]` runs [`otui_lsp_server::cli::run_check`] — the same
//! widget-aware diagnostics the server publishes, built from the same workspace-scanned indexes, so
//! the two can never disagree on the same corpus. Any other first argument (including none, and any
//! flag an editor might pass on launch) falls through to the LSP server unchanged — the
//! no-subcommand path is the one every client relies on and must never change shape.

use std::error::Error;

use lsp_server::{Connection, Notification};
use lsp_types::InitializeParams;
use otui_lsp_server::{Backend, serve};

const USAGE: &str = "otui-lsp — Language Server (LSP 3.17) for OTUI/OTML\n\n\
Usage:\n  \
otui-lsp                                        run the language server over stdio\n  \
otui-lsp fmt <paths...> [--check|--write]       format .otui files (default: --check)\n  \
otui-lsp check <paths...> [--deny <level>]      lint .otui/.otmod/.otfont + asset refs (default: --deny error)\n  \
otui-lsp --help | -h                            print this message\n  \
otui-lsp --version | -V                         print the version";

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    match std::env::args().nth(1).as_deref() {
        Some("fmt") => {
            let code = otui_lsp_server::cli::run_fmt(std::env::args().skip(2));
            // `ExitCode` is deliberately opaque (no public conversion to `i32`); `run_fmt` only
            // ever produces `SUCCESS`/`FAILURE`, so an equality check against `SUCCESS` is enough
            // to pick the process exit status.
            std::process::exit(i32::from(code != std::process::ExitCode::SUCCESS));
        }
        Some("check") => {
            let code = otui_lsp_server::cli::run_check(std::env::args().skip(2));
            // Mirrors the `fmt` arm above: `run_check` only ever produces `SUCCESS`/`FAILURE`.
            std::process::exit(i32::from(code != std::process::ExitCode::SUCCESS));
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        Some("--version" | "-V") => {
            println!("otui-lsp {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        // Anything else — including `None` (no arguments, how every editor launches it) and any
        // flag an editor might pass — falls through to the existing LSP server, unchanged.
        _ => {}
    }

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

    // Tell the background workspace scan (if still running) to stop, so it drops its `Sender` clone
    // promptly and does not make `IoThreads::join` below wait for a full scan.
    backend.signal_shutdown();

    // Drop everything holding a `Sender<Message>` (the backend's clone and the connection itself)
    // BEFORE joining the I/O threads: `IoThreads::join` waits for the stdio writer, which only
    // finishes once every sender to its channel is dropped. Leaving these alive would hang `join`
    // before we ever reach `process::exit`.
    drop(backend);
    drop(connection);

    io_threads.join()?;
    std::process::exit(termination.exit_code());
}
