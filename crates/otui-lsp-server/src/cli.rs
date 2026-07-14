//! `otui-lsp fmt` — a small CLI wrapper around [`otui_core::format::format`], rust-analyzer-style
//! (a subcommand of the same binary that runs the LSP server with no arguments).
//!
//! This module owns all the I/O (argument parsing, directory walking, file reads/writes) that
//! `otui-core` is not allowed to do; the actual formatting decision is entirely
//! [`otui_core::format::format`]'s — this file never re-implements or second-guesses it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use lsp_types::Uri;

use crate::collect_files_under;

/// What to do with a file whose formatted text differs from its current text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Report files that need formatting; do not touch them (the default).
    Check,
    /// Rewrite files that need formatting in place.
    Write,
}

/// Parsed `fmt` invocation.
struct FmtArgs {
    mode: Mode,
    paths: Vec<PathBuf>,
}

/// Parse `fmt` subcommand arguments: positional paths plus the mutually exclusive `--check` /
/// `--write` flags. No flag given defaults to `--check`.
fn parse_args(args: impl Iterator<Item = String>) -> Result<FmtArgs, String> {
    let mut mode: Option<Mode> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--check" => {
                if mode == Some(Mode::Write) {
                    return Err("`--check` and `--write` are mutually exclusive".to_owned());
                }
                mode = Some(Mode::Check);
            }
            "--write" => {
                if mode == Some(Mode::Check) {
                    return Err("`--check` and `--write` are mutually exclusive".to_owned());
                }
                mode = Some(Mode::Write);
            }
            other if other.starts_with('-') => {
                return Err(format!("unexpected flag '{other}'"));
            }
            other => paths.push(PathBuf::from(other)),
        }
    }
    if paths.is_empty() {
        return Err("no path given. Usage: otui-lsp fmt <paths...> [--check|--write]".to_owned());
    }
    Ok(FmtArgs {
        mode: mode.unwrap_or(Mode::Check),
        paths,
    })
}

/// `otui-lsp fmt <paths...> [--check|--write]`.
///
/// Expands each directory argument into the `.otui` files under it (via [`collect_files_under`],
/// shared with the server's workspace scan); a file argument is taken as-is when it ends in
/// `.otui`. Targets are de-duplicated and processed in sorted order so output is deterministic.
///
/// `--check` (the default when neither flag is given) never touches disk: it reports every file
/// whose [`otui_core::format::format`] output differs from its current text and exits
/// [`ExitCode::FAILURE`] iff at least one such file exists. `--write` rewrites those files in
/// place and always exits [`ExitCode::SUCCESS`] (barring an I/O error). Either way, a file that
/// does not parse cleanly (`format` returns `None`) is reported as skipped and never counts
/// against `--check` — a syntax error is the linter's job, not the formatter's.
#[must_use]
pub fn run_fmt(args: impl Iterator<Item = String>) -> ExitCode {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("otui-lsp fmt: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let targets = match resolve_targets(&parsed.paths) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("otui-lsp fmt: {msg}");
            return ExitCode::FAILURE;
        }
    };

    match parsed.mode {
        Mode::Check => run_check(&targets),
        Mode::Write => run_write(&targets),
    }
}

/// Expand `paths` (files and/or directories) into a sorted, de-duplicated list of `.otui` file
/// paths. A directory is walked via [`collect_files_under`] (extension `otui`); a file argument
/// is accepted as-is only when it ends in `.otui`. Errors (a path that does not exist, or a
/// non-`.otui` file given directly) are surfaced immediately rather than silently skipped, so a
/// typo'd path is never mistaken for "found zero files".
fn resolve_targets(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        // Canonicalize to an absolute path FIRST. `collect_files_under` keys its results by a
        // `file://` URL (via `url::Url::from_file_path`, which rejects relative paths), so a
        // relative directory argument — the primary CI invocation `fmt --check .` — would otherwise
        // yield zero files and a dangerous false-clean SUCCESS. Canonicalizing also gives a stable,
        // deduplicable path so the same file reached two ways collapses to one target. The error
        // keeps the user's typed path for a readable message.
        let abs = std::fs::canonicalize(path)
            .map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
        if abs.is_dir() {
            let mut found: HashMap<Uri, String> = HashMap::new();
            collect_files_under(&abs, "otui", &mut found);
            for uri in found.into_keys() {
                if let Some(fs_path) = crate::uri_to_file_path(&uri) {
                    out.push(fs_path);
                }
            }
        } else if abs.extension().is_some_and(|e| e == "otui") {
            out.push(abs);
        } else {
            return Err(format!(
                "'{}' is neither a directory nor a `.otui` file",
                path.display()
            ));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `--check`: report every target whose formatted text differs from its current text; never
/// writes. Exits [`ExitCode::FAILURE`] iff at least one file needs formatting.
fn run_check(targets: &[PathBuf]) -> ExitCode {
    let mut needs_formatting = 0usize;
    let mut skipped = 0usize;
    for path in targets {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("otui-lsp fmt: cannot read '{}': {e}", path.display());
                return ExitCode::FAILURE;
            }
        };
        match otui_core::format::format(&text) {
            Some(formatted) if formatted != text => {
                println!("needs formatting: {}", path.display());
                needs_formatting += 1;
            }
            Some(_) => {}
            None => {
                eprintln!("skipped (syntax errors): {}", path.display());
                skipped += 1;
            }
        }
    }
    println!("{needs_formatting} file(s) need formatting ({skipped} skipped)");
    if needs_formatting > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `--write`: rewrite every target whose formatted text differs from its current text; always
/// succeeds (barring an I/O error) since a diff found is a diff fixed, not a failure.
fn run_write(targets: &[PathBuf]) -> ExitCode {
    let mut formatted_count = 0usize;
    let mut skipped = 0usize;
    for path in targets {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("otui-lsp fmt: cannot read '{}': {e}", path.display());
                return ExitCode::FAILURE;
            }
        };
        match otui_core::format::format(&text) {
            Some(formatted) if formatted != text => {
                if let Err(e) = std::fs::write(path, &formatted) {
                    eprintln!("otui-lsp fmt: cannot write '{}': {e}", path.display());
                    return ExitCode::FAILURE;
                }
                println!("formatted: {}", path.display());
                formatted_count += 1;
            }
            Some(_) => {}
            None => {
                eprintln!("skipped (syntax errors): {}", path.display());
                skipped += 1;
            }
        }
    }
    println!("{formatted_count} formatted, {skipped} skipped");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but syntactically valid `.otui` document with an over-indented property line —
    /// `format` should re-indent it to 2 spaces, so it "needs formatting".
    const MISINDENTED: &str = "MyWidget\n      id: foo\n";

    /// The canonical form of [`MISINDENTED`] — already what `format` would produce.
    const CANONICAL: &str = "MyWidget\n  id: foo\n";

    /// An unterminated inline array — a genuine `ERROR` node in the parse tree (mirroring
    /// `otui_core::format`'s own `a_document_with_a_parse_error_yields_none` fixture), so `format`
    /// returns `None` for this document per its hard safety gate.
    const UNPARSEABLE: &str = "MyWidget\n  x: [a, b\n";

    /// A scratch directory unique to this test process + case name, under the OS temp dir.
    fn scratch_dir(case: &str) -> PathBuf {
        std::env::temp_dir().join(format!("otui-lsp-cli-test-{}-{case}", std::process::id()))
    }

    #[test]
    fn check_flags_a_misindented_file_as_needing_formatting() {
        let dir = scratch_dir("misindented-check");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("widget.otui");
        std::fs::write(&file, MISINDENTED).expect("write fixture");

        let code = run_fmt(vec!["--check".to_owned(), dir.display().to_string()].into_iter());
        assert_eq!(code, ExitCode::FAILURE);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_formats_in_place_and_is_then_idempotent() {
        let dir = scratch_dir("write-idempotent");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("widget.otui");
        std::fs::write(&file, MISINDENTED).expect("write fixture");

        let code = run_fmt(vec!["--write".to_owned(), dir.display().to_string()].into_iter());
        assert_eq!(code, ExitCode::SUCCESS);

        let on_disk = std::fs::read_to_string(&file).expect("read back");
        let expected = otui_core::format::format(MISINDENTED).expect("misindented file parses");
        assert_eq!(on_disk, expected);

        // A second `--check` on the now-formatted file must report clean (idempotent).
        let code = run_fmt(vec!["--check".to_owned(), dir.display().to_string()].into_iter());
        assert_eq!(code, ExitCode::SUCCESS);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_on_an_already_canonical_file_succeeds() {
        let dir = scratch_dir("canonical-check");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("widget.otui");
        std::fs::write(&file, CANONICAL).expect("write fixture");

        let code = run_fmt(vec!["--check".to_owned(), dir.display().to_string()].into_iter());
        assert_eq!(code, ExitCode::SUCCESS);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unparseable_file_is_skipped_not_flagged() {
        let dir = scratch_dir("unparseable");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("widget.otui");
        std::fs::write(&file, UNPARSEABLE).expect("write fixture");

        // Sanity: this really is unparseable per `format`'s hard safety gate.
        assert!(otui_core::format::format(UNPARSEABLE).is_none());

        let code = run_fmt(vec!["--check".to_owned(), dir.display().to_string()].into_iter());
        assert_eq!(code, ExitCode::SUCCESS);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn conflicting_flags_are_rejected() {
        let code = run_fmt(
            vec![
                "--check".to_owned(),
                "--write".to_owned(),
                "some.otui".to_owned(),
            ]
            .into_iter(),
        );
        assert_eq!(code, ExitCode::FAILURE);
    }

    #[test]
    fn no_paths_is_rejected() {
        let code = run_fmt(vec!["--check".to_owned()].into_iter());
        assert_eq!(code, ExitCode::FAILURE);
    }
}
