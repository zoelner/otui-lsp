//! The `otui-lsp` language server binary.
//!
//! This is the thin transport shell: it speaks LSP 3.17 over stdio (JSON-RPC 2.0) and delegates
//! all language semantics to [`otui_core`] via the [`lang_api::LanguageService`] contract, doing
//! only the byte-offset ↔ protocol-position conversion at the boundary.
//!
//! The full server lifecycle (`initialize` / `initialized` / `shutdown` / `exit`, capability
//! negotiation) lands in M3. This M1 scaffold just proves the binary wires up against the core.

use lang_api::LanguageService;
use otui_core::OtuiService;

fn main() {
    let service = OtuiService::new();
    // Placeholder until the tower-lsp stdio loop is wired in M3.
    eprintln!(
        "otui-lsp {} — language backend '{}' ready (LSP transport lands in M3)",
        env!("CARGO_PKG_VERSION"),
        service.language_id(),
    );
}
