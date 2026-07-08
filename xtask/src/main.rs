//! `cargo xtask` — development tasks for otui-lsp.
//!
//! Currently a stub. The first real task (M4) generates the per-fork property/color/widget
//! catalog by reading the OTClient engine source (`uiwidgetbasestyle.cpp`, `uitranslator.cpp`,
//! `color.cpp`) and emitting a data file consumed by `otui-core::schema`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("gen-catalog") => {
            eprintln!("xtask gen-catalog: not implemented yet (lands in M4).");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("xtask: unknown task '{other}'. Available: gen-catalog");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("usage: cargo xtask <task>\n  tasks:\n    gen-catalog   generate the OTUI schema catalog (M4)");
            ExitCode::FAILURE
        }
    }
}
