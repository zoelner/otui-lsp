//! `otui-lsp fmt` — a small CLI wrapper around [`otui_core::format::format`], rust-analyzer-style
//! (a subcommand of the same binary that runs the LSP server with no arguments).
//!
//! This module owns all the I/O (argument parsing, directory walking, file reads/writes) that
//! `otui-core` is not allowed to do; the actual formatting decision is entirely
//! [`otui_core::format::format`]'s — this file never re-implements or second-guesses it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::RwLock;

use lang_api::Severity;
use lsp_types::{DiagnosticSeverity, NumberOrString, Uri};
use otui_core::manifest::{analyze_font_manifest, analyze_manifest};

use crate::position::{LineIndex, PositionEncoding};
use crate::{
    build_indexes, collect_paths_under, detect_client_roots, missing_asset_diagnostics,
    uri_from_file_path,
};

/// A manifest analyzer: `analyze_manifest` (`.otmod`) or `analyze_font_manifest` (`.otfont`), both
/// `&str -> Vec<Diagnostic>`. Aliased so the dispatch table's type stays readable (clippy).
type ManifestAnalyzer = fn(&str) -> Vec<lang_api::Diagnostic>;

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

    let targets = match resolve_targets(&parsed.paths, &[]) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("otui-lsp fmt: {msg}");
            return ExitCode::FAILURE;
        }
    };

    match parsed.mode {
        Mode::Check => run_fmt_check(&targets),
        Mode::Write => run_write(&targets),
    }
}

/// Expand `paths` (files and/or directories) into a sorted, de-duplicated list of `.otui` file
/// paths. A directory is walked via [`collect_paths_under`] (extension `otui`) — a path-only walk
/// that lists every matching file regardless of whether it can later be read, so an
/// unreadable/oversized/binary `.otui` under the directory still becomes a target instead of being
/// silently dropped from discovery (its per-target read below is what then reports the failure). A
/// file argument is accepted as-is only when it ends in `.otui`. Errors (a path that does not exist,
/// or a non-`.otui` file given directly) are surfaced immediately rather than silently skipped, so a
/// typo'd path is never mistaken for "found zero files".
fn resolve_targets(paths: &[PathBuf], tolerate_exts: &[&str]) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        // Canonicalize to an absolute path FIRST. A relative directory argument — the primary CI
        // invocation `fmt --check .` — would otherwise yield an unstable path. Canonicalizing also
        // gives a stable, deduplicable path so the same file reached two ways collapses to one
        // target. The error keeps the user's typed path for a readable message.
        let abs = std::fs::canonicalize(path)
            .map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
        if abs.is_dir() {
            collect_paths_under(&abs, "otui", &mut out)?;
        } else if abs
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("otui"))
        {
            out.push(abs);
        } else if abs
            .extension()
            .is_some_and(|e| tolerate_exts.iter().any(|t| e.eq_ignore_ascii_case(t)))
        {
            // A direct file with a companion extension the caller collects separately (e.g. a
            // `.otmod`/`.otfont` manifest for `check`): tolerate it here — a later pass picks it up —
            // rather than rejecting it as "not a `.otui` file".
            continue;
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

/// `fmt --check`: report every target whose formatted text differs from its current text; never
/// writes. Exits [`ExitCode::FAILURE`] iff at least one file needs formatting.
///
/// Named `run_fmt_check` (not `run_check`) to keep it distinct from the top-level [`run_check`] —
/// the *linter* `check` subcommand — which is an entirely different pass (diagnostics, not
/// formatting) that happens to share the word "check".
fn run_fmt_check(targets: &[PathBuf]) -> ExitCode {
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

/// The `--deny` threshold: which diagnostic [`Severity`]s cause `check` to exit non-zero. `error`
/// (the default) is the least strict — spec §2.10's stance that the LSP is never stricter than the
/// engine unless asked to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DenyLevel {
    /// Fail on `Severity::Error` only (the default).
    Error,
    /// Fail on `Severity::Error` or `Severity::Warning`.
    Warnings,
    /// Fail on any diagnostic at all, including a `Severity::Hint`.
    Hints,
}

impl DenyLevel {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "error" => Ok(Self::Error),
            "warnings" => Ok(Self::Warnings),
            "hints" => Ok(Self::Hints),
            other => Err(format!(
                "unknown --deny level '{other}' (expected error|warnings|hints)"
            )),
        }
    }

    /// Whether a finding tally at this level should make `check` exit non-zero.
    fn fails(self, errors: usize, warnings: usize, hints: usize) -> bool {
        match self {
            Self::Error => errors > 0,
            Self::Warnings => errors > 0 || warnings > 0,
            Self::Hints => errors > 0 || warnings > 0 || hints > 0,
        }
    }
}

/// Parsed `check` invocation.
#[derive(Debug)]
struct CheckArgs {
    paths: Vec<PathBuf>,
    deny: DenyLevel,
}

/// Parse `check` subcommand arguments: positional paths plus `--deny <level>` (`error` — the
/// default — `warnings`, or `hints`). Accepts both `--deny <level>` (two tokens) and `--deny=<level>`
/// (one token).
fn parse_check_args(args: impl Iterator<Item = String>) -> Result<CheckArgs, String> {
    let mut deny: Option<DenyLevel> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--deny" {
            let value = iter
                .next()
                .ok_or_else(|| "`--deny` requires a value (error|warnings|hints)".to_owned())?;
            deny = Some(DenyLevel::parse(&value)?);
        } else if let Some(value) = arg.strip_prefix("--deny=") {
            deny = Some(DenyLevel::parse(value)?);
        } else if arg.starts_with('-') {
            return Err(format!("unexpected flag '{arg}'"));
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    if paths.is_empty() {
        return Err(
            "no path given. Usage: otui-lsp check <paths...> [--deny error|warnings|hints]"
                .to_owned(),
        );
    }
    Ok(CheckArgs {
        paths,
        deny: deny.unwrap_or(DenyLevel::Error),
    })
}

/// The directory `check`'s workspace-index scan should be rooted at for each of `target_dirs` (each
/// already a directory: the linted path itself, or its parent for a file argument) — the client
/// **install** root when one is discoverable by walking up from that directory
/// ([`detect_client_roots`]), falling back to that *same* directory itself when none is found for
/// it.
///
/// The fallback is applied **per target directory**, not to the aggregate list: an earlier version
/// fell back to `target_dirs` only when the combined `roots` across every target was empty, so a
/// single target with a discoverable client root made the aggregate non-empty and silently starved
/// every *other*, client-root-less target of a root of its own — a loose standalone module passed
/// alongside a real client checkout was then never indexed at all (its files got no
/// `StyleIndex`/`LuaWidgetIndex` scan of their own containing project; they were instead lumped in
/// with — and diagnosed against — the unrelated client's roots). Resolving each directory
/// independently — its own client root, or itself — guarantees every target directory contributes at
/// least one scan root of its own.
///
/// Each per-directory lookup passes an **empty** fallback pool to [`detect_client_roots`] (rather
/// than `target_dirs`), deliberately not reusing that function's own "no client root found near this
/// doc — scan every workspace root for one" fallback: that fallback exists for a caller with no
/// single anchor document (the workspace-scan completion refresh), and reusing it here would let it
/// walk into a *sibling* target's client root — exactly the cross-target leak this fix removes — so
/// `dir` alone stands in for the whole fallback pool, isolating each target's discovery from every
/// other's.
///
/// This is the fidelity-critical step: linting even a single `.otui` file must still build the
/// workspace [`otui_core::style_index::StyleIndex`]/[`otui_core::lua_widgets::LuaWidgetIndex`] from
/// its **containing project** (every sibling `.otui`/`.lua` file `build_indexes` can reach from the
/// discovered root), not just the one file being linted — otherwise a widget class only ever
/// declared in a Lua module (spec §2.3's OTUI↔Lua bridge) would be invisible, and any custom
/// property it declares would be misreported as `unknown-property` (the regression the
/// widget-aware diagnostics wave fixed for the server; see `build_indexes`'s doc comment for why
/// the server and this CLI must never diverge here).
fn discover_roots(target_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for dir in target_dirs {
        let found = detect_client_roots(Some(dir), &[]);
        if found.is_empty() {
            if !roots.contains(dir) {
                roots.push(dir.clone());
            }
        } else {
            for root in found {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
    }
    roots
}

/// The human-readable label for a [`Severity`] in `check`'s rustc-style output — lowercase, matching
/// `error[E0000]:`-style compiler diagnostics rather than the `Diagnostic` enum's `Debug` casing.
fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Hint => "hint",
    }
}

/// One rendered finding: an already-resolved 1-based `(line, column)` plus the diagnostic's
/// severity/code/message, ready to print and to sort by.
struct Finding {
    path: PathBuf,
    line: u32,
    column: u32,
    severity: Severity,
    code: String,
    message: String,
}

/// Push a finding and bump the `[errors, warnings, hints]` tally in one place, so the exit-code
/// decision and the printed summary can never disagree about what was found.
#[allow(clippy::too_many_arguments)]
fn record(
    findings: &mut Vec<Finding>,
    counts: &mut [usize; 3],
    path: &Path,
    line: u32,
    column: u32,
    severity: Severity,
    code: String,
    message: String,
) {
    counts[match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Hint => 2,
    }] += 1;
    findings.push(Finding {
        path: path.to_path_buf(),
        line,
        column,
        severity,
        code,
        message,
    });
}

/// Map an `lsp_types` diagnostic severity into `lang_api`'s (the CLI's tally vocabulary).
/// `INFORMATION` and an absent severity both collapse to `Hint` — the least-strict bucket (§2.10).
fn lsp_severity(severity: Option<DiagnosticSeverity>) -> Severity {
    match severity {
        Some(DiagnosticSeverity::ERROR) => Severity::Error,
        Some(DiagnosticSeverity::WARNING) => Severity::Warning,
        _ => Severity::Hint,
    }
}

/// The stable code string of an `lsp_types` diagnostic (`missing-asset` in practice).
fn lsp_code(code: Option<NumberOrString>) -> String {
    match code {
        Some(NumberOrString::String(s)) => s,
        Some(NumberOrString::Number(n)) => n.to_string(),
        None => String::new(),
    }
}

/// Every `.<ext>` file under `paths` (matched case-insensitively — `.OTMOD`/`.otmod` are the same
/// target) — directories walked recursively via [`collect_paths_under`] (a path-only walk, so an
/// unreadable/oversized/binary manifest is still discovered as a target rather than silently
/// dropped; an unreadable directory is a hard error here too, propagated rather than swallowed),
/// a file argument included only when it already carries that extension. Unlike [`resolve_targets`]
/// this never errors on a non-matching direct file (each extension is collected by its own pass).
/// `paths` are already canonicalized (absolute).
fn collect_by_ext(paths: &[PathBuf], ext: &str) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_paths_under(path, ext, &mut out)?;
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            out.push(path.clone());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `otui-lsp check <paths...> [--deny error|warnings|hints]`.
///
/// A CLI-native linter, rust-analyzer-`check`-style: for every target `.otui` file (expanded from
/// `paths` exactly like [`run_fmt`]'s [`resolve_targets`]) it computes the same widget-aware
/// diagnostics the server publishes on open/change
/// ([`otui_core::OtuiService::diagnostics_with_widgets_and_links`]), fed the workspace
/// [`otui_core::style_index::StyleIndex`]/[`otui_core::lua_widgets::LuaWidgetIndex`] built by the
/// exact same [`build_indexes`] the server's own initial scan uses — see its doc comment for why
/// that sharing is load-bearing, not incidental.
///
/// Prints one line per finding in rustc's `path:line:col: severity[code]: message` shape (1-based
/// line/column, byte columns — see [`LineIndex`]/[`PositionEncoding::Utf8`]), sorted by path then
/// line then column for deterministic output, followed by a one-line summary. Exit code is governed
/// entirely by [`DenyLevel`] against each [`lang_api::Diagnostic::severity`] — never a hard-coded
/// diagnostic code list, so a future diagnostic's severity is automatically honored here without a
/// matching update to this file (spec §2.10: the LSP is never *stricter* than the engine unless a
/// caller opts into it via `--deny`).
#[must_use]
pub fn run_check(args: impl Iterator<Item = String>) -> ExitCode {
    let parsed = match parse_check_args(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("otui-lsp check: {msg}");
            return ExitCode::FAILURE;
        }
    };

    // Canonicalize up front (mirrors `resolve_targets`): a relative argument must resolve to a
    // stable, absolute path both for the target walk below and for the client-root ancestor walk,
    // and a typo'd path must surface as an error rather than a silent "found zero files".
    let mut canonical_paths: Vec<PathBuf> = Vec::with_capacity(parsed.paths.len());
    for path in &parsed.paths {
        match std::fs::canonicalize(path) {
            Ok(abs) => canonical_paths.push(abs),
            Err(e) => {
                eprintln!("otui-lsp check: cannot read '{}': {e}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }

    // The directory each canonical path resolves to (itself if already a directory, its parent for
    // a file argument) — the anchor `discover_roots` walks up from.
    let mut target_dirs: Vec<PathBuf> = Vec::new();
    for path in &canonical_paths {
        let dir = if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.clone())
        };
        if !target_dirs.contains(&dir) {
            target_dirs.push(dir);
        }
    }

    let root_paths = discover_roots(&target_dirs);
    let roots: Vec<Uri> = root_paths
        .iter()
        .filter_map(|p| uri_from_file_path(p))
        .collect();

    // The single canonical index-building path shared with the server — see `build_indexes`'s doc
    // comment for why the two can never observe a different corpus for the same files on disk. The
    // CLI is a one-shot process with nothing to cancel for, so it passes a never-stop closure —
    // `build_indexes` always runs to completion here.
    let built = build_indexes(&roots, &|| false);

    // `check` also lints `.otmod`/`.otfont` manifests (collected separately below), so a manifest
    // file passed directly must be tolerated here rather than rejected as "not a `.otui` file".
    let targets = match resolve_targets(&canonical_paths, &["otmod", "otfont"]) {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("otui-lsp check: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let service = otui_core::OtuiService::new();
    let otpkg_cache: RwLock<HashMap<PathBuf, bool>> = RwLock::new(HashMap::new());
    let mut findings: Vec<Finding> = Vec::new();
    // [errors, warnings, hints] — the exit-code tally, driven purely by severity (never a code list).
    let mut counts = [0usize; 3];

    // `.otui` widget diagnostics (the same widget-aware pass the server publishes) plus the
    // asset-reference existence check the server layers on top of it — so `check` is not silently
    // weaker than the editor (a broken `image-source:` is a `missing-asset` warning here too).
    for path in &targets {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("otui-lsp check: cannot read '{}': {e}", path.display());
                return ExitCode::FAILURE;
            }
        };
        let (diagnostics, asset_links) =
            service.diagnostics_with_widgets_and_links(&text, &built.style_index, &built.lua_index);
        let line_index = LineIndex::new(&text);
        for diagnostic in diagnostics {
            let pos = line_index.position(diagnostic.span.start, PositionEncoding::Utf8);
            record(
                &mut findings,
                &mut counts,
                path,
                pos.line + 1,
                pos.character + 1,
                diagnostic.severity,
                diagnostic.code.to_owned(),
                diagnostic.message,
            );
        }
        // Asset-existence findings come back as `lsp_types::Diagnostic` (already position-resolved
        // in the encoding we pass); normalize each into the same severity/code/message shape.
        let doc_dir = path.parent();
        for lsp_diag in missing_asset_diagnostics(
            asset_links,
            &text,
            doc_dir,
            &root_paths,
            &otpkg_cache,
            PositionEncoding::Utf8,
        ) {
            record(
                &mut findings,
                &mut counts,
                path,
                lsp_diag.range.start.line + 1,
                lsp_diag.range.start.character + 1,
                lsp_severity(lsp_diag.severity),
                lsp_code(lsp_diag.code),
                lsp_diag.message,
            );
        }
    }

    // Manifest diagnostics: a malformed `.otmod`/`.otfont` (`missing-module-root`/`missing-font-root`
    // are Errors) must fail `check` too — the same `analyze_manifest`/`analyze_font_manifest` the
    // server runs on manifest open/change, returning the same `lang_api::Diagnostic` shape.
    let manifests: [(&str, ManifestAnalyzer); 2] = [
        ("otmod", analyze_manifest),
        ("otfont", analyze_font_manifest),
    ];
    for (ext, analyze) in manifests {
        let ext_targets = match collect_by_ext(&canonical_paths, ext) {
            Ok(t) => t,
            Err(msg) => {
                eprintln!("otui-lsp check: {msg}");
                return ExitCode::FAILURE;
            }
        };
        for path in ext_targets {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("otui-lsp check: cannot read '{}': {e}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            let line_index = LineIndex::new(&text);
            for diagnostic in analyze(&text) {
                let pos = line_index.position(diagnostic.span.start, PositionEncoding::Utf8);
                record(
                    &mut findings,
                    &mut counts,
                    &path,
                    pos.line + 1,
                    pos.character + 1,
                    diagnostic.severity,
                    diagnostic.code.to_owned(),
                    diagnostic.message,
                );
            }
        }
    }

    let [error_count, warning_count, hint_count] = counts;

    findings.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });
    for finding in &findings {
        println!(
            "{}:{}:{}: {}[{}]: {}",
            finding.path.display(),
            finding.line,
            finding.column,
            severity_label(finding.severity),
            finding.code,
            finding.message
        );
    }
    println!("{error_count} error(s), {warning_count} warning(s), {hint_count} hint(s)");

    if parsed.deny.fails(error_count, warning_count, hint_count) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
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

    // --- `check` argument parsing -------------------------------------------------------------

    #[test]
    fn check_args_default_to_deny_error() {
        let parsed = parse_check_args(vec!["a.otui".to_owned()].into_iter()).expect("parses");
        assert_eq!(parsed.deny, DenyLevel::Error);
        assert_eq!(parsed.paths, vec![PathBuf::from("a.otui")]);
    }

    #[test]
    fn check_args_accept_deny_as_two_tokens_and_as_one() {
        let parsed = parse_check_args(
            vec![
                "a.otui".to_owned(),
                "--deny".to_owned(),
                "warnings".to_owned(),
            ]
            .into_iter(),
        )
        .expect("parses");
        assert_eq!(parsed.deny, DenyLevel::Warnings);

        let parsed =
            parse_check_args(vec!["a.otui".to_owned(), "--deny=hints".to_owned()].into_iter())
                .expect("parses");
        assert_eq!(parsed.deny, DenyLevel::Hints);
    }

    #[test]
    fn check_args_reject_an_unknown_deny_level() {
        let err = parse_check_args(
            vec![
                "a.otui".to_owned(),
                "--deny".to_owned(),
                "everything".to_owned(),
            ]
            .into_iter(),
        )
        .expect_err("unknown level must be rejected");
        assert!(
            err.contains("everything"),
            "error should name the bad value: {err}"
        );
    }

    #[test]
    fn check_args_reject_deny_with_no_value() {
        let err = parse_check_args(vec!["a.otui".to_owned(), "--deny".to_owned()].into_iter())
            .expect_err("a trailing --deny with no value must be rejected");
        assert!(err.contains("--deny"));
    }

    #[test]
    fn check_args_reject_no_paths() {
        assert!(parse_check_args(std::iter::empty()).is_err());
    }

    #[test]
    fn check_args_reject_an_unrecognized_flag() {
        assert!(parse_check_args(vec!["--bogus".to_owned()].into_iter()).is_err());
    }

    // --- `DenyLevel::fails` — the exit-code gate ----------------------------------------------
    //
    // These pin the exact matrix `run_check` relies on to turn a severity tally into an exit
    // code. The mandatory revert experiment for this node breaks the threshold by hard-coding it
    // to compare against `errors > 0` regardless of `self` — `hint_only_finding_fails_only_under_deny_hints`
    // is the one that goes red under that mutation (a hint-only tally must NOT fail under
    // `Error`/`Warnings`, only under `Hints`).

    #[test]
    fn error_only_finding_fails_under_every_deny_level() {
        for level in [DenyLevel::Error, DenyLevel::Warnings, DenyLevel::Hints] {
            assert!(level.fails(1, 0, 0), "{level:?} must fail on a bare error");
        }
    }

    #[test]
    fn warning_only_finding_fails_only_under_warnings_and_hints() {
        assert!(!DenyLevel::Error.fails(0, 1, 0));
        assert!(DenyLevel::Warnings.fails(0, 1, 0));
        assert!(DenyLevel::Hints.fails(0, 1, 0));
    }

    #[test]
    fn hint_only_finding_fails_only_under_deny_hints() {
        assert!(!DenyLevel::Error.fails(0, 0, 1));
        assert!(!DenyLevel::Warnings.fails(0, 0, 1));
        assert!(DenyLevel::Hints.fails(0, 0, 1));
    }

    #[test]
    fn a_clean_tally_never_fails_at_any_level() {
        for level in [DenyLevel::Error, DenyLevel::Warnings, DenyLevel::Hints] {
            assert!(
                !level.fails(0, 0, 0),
                "{level:?} must not fail on a clean tally"
            );
        }
    }

    // --- `discover_roots` — the workspace-root discovery for `check` -------------------------

    #[test]
    fn discover_roots_falls_back_to_the_given_dirs_with_no_client_root_anywhere() {
        // A loose scratch directory with none of `init.lua`/`data`/`modules` above it: no client
        // root is discoverable, so `discover_roots` must fall back to the given directory itself
        // (still scanned) rather than returning empty (which would scan nothing at all).
        let dir = scratch_dir("discover-roots-fallback");
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let roots = discover_roots(std::slice::from_ref(&dir));
        assert_eq!(roots, vec![dir.clone()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discover_roots_does_not_drop_a_loose_target_when_another_target_has_a_client_root() {
        // Regression (CodeRabbit): the aggregate `roots` used to be checked for emptiness only
        // ONCE, across every target combined, so a single target with a discoverable client root
        // made that aggregate non-empty and silently starved every *other*, client-root-less
        // target of a root of its own — that target (and every file under it) was then never
        // indexed at all. Two independent target directories here: `client_root_target` sits
        // (nested) under a real client install (`init.lua` + `data/` + `modules/`); `loose_dir` has
        // none of those anywhere above it and has no relation to the first tree at all.
        let base = scratch_dir("discover-roots-per-target");
        let client_root = base.join("client");
        let nested_module = client_root.join("modules").join("m");
        std::fs::create_dir_all(&nested_module).expect("create nested module dir");
        std::fs::create_dir_all(client_root.join("data")).expect("create data dir");
        std::fs::write(client_root.join("init.lua"), "require('nothing')\n")
            .expect("write init.lua");

        let loose_dir = base.join("loose");
        std::fs::create_dir_all(&loose_dir).expect("create loose dir");

        let roots = discover_roots(&[nested_module, loose_dir.clone()]);

        assert!(
            roots.contains(&loose_dir),
            "the loose, client-root-less target must still get its own scan root: {roots:?}"
        );
        assert!(
            roots.contains(&client_root),
            "the other target's own client root must still be discovered: {roots:?}"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    // --- `run_check` — the full pass, in-process (see `tests/cli_check.rs` for the compiled-binary
    // end-to-end coverage of the same scenarios, including the widget-aware fidelity case) --------

    #[test]
    fn run_check_reports_an_error_and_exits_nonzero_by_default() {
        let dir = scratch_dir("check-error");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(
            dir.join("widget.otui"),
            "MyWidget < UIWidget\n  anchors.lft: parent\n",
        )
        .expect("write fixture");

        let code = run_check(vec![dir.display().to_string()].into_iter());
        assert_eq!(code, ExitCode::FAILURE);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_check_a_hint_only_project_exits_zero_by_default_and_nonzero_under_deny_hints() {
        let dir = scratch_dir("check-hint-only");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("widget.otui"), "MyPanel < UIWidget\n  widht: 10\n")
            .expect("write fixture");

        let code = run_check(vec![dir.display().to_string()].into_iter());
        assert_eq!(code, ExitCode::SUCCESS);

        let code = run_check(
            vec![
                dir.display().to_string(),
                "--deny".to_owned(),
                "hints".to_owned(),
            ]
            .into_iter(),
        );
        assert_eq!(code, ExitCode::FAILURE);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_check_on_a_nonexistent_path_fails() {
        let code = run_check(vec!["/does/not/exist.otui".to_owned()].into_iter());
        assert_eq!(code, ExitCode::FAILURE);
    }
}
