//! SARIF 2.1.0 emitter for `otui-lsp check --format sarif`.
//!
//! GitHub code-scanning (`github/codeql-action/upload-sarif`) ingests a SARIF log and maps its
//! findings onto pull-request annotations keyed by a **relative** `artifactLocation.uri`; this
//! module's only job is to turn `check`'s already-computed [`super::Finding`]s into that shape —
//! it never re-runs or re-interprets the lint, so the SARIF and human reports can never disagree
//! about *what* was found, only how it is rendered.

use std::path::{Path, PathBuf};

use lang_api::Severity;
use serde_json::{Value, json};

use super::Finding;

/// Map a [`Severity`] to SARIF's `result.level` vocabulary. SARIF has no direct "hint" level;
/// `note` is its closest analogue (the least severe of the three GitHub renders), matching
/// `Severity::Hint`'s own "silently ignored by the engine, surfaced only as a nudge" tolerance
/// (spec §2.10).
fn severity_level(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Hint => "note",
    }
}

/// `path` relative to `cwd` (the repo checkout root a CI action runs `check` from) when it falls
/// under it, else relative to whichever of `roots` it falls under (first match wins), else the
/// bare file name — forward-slashed either way.
///
/// `cwd` is checked **first**, ahead of `roots`: GitHub code-scanning's `upload-sarif` action maps
/// `artifactLocation.uri` onto the repository tree relative to wherever the action runs, i.e. the
/// process's current directory — NOT relative to whatever narrower workspace root `check` happened
/// to discover for a given target. A target directory nested below the repo root (e.g. `check`ing
/// a `ui/` subdirectory, whose own `detect_client_roots` walk lands on `ui/` itself) must still
/// produce a repo-root-relative URI (`ui/widget.otui`), not one relative to that narrower root
/// (`widget.otui`) — the latter would misalign every annotation GitHub renders on the PR diff.
///
/// This is load-bearing, not cosmetic: an absolute path would both fail that mapping AND leak the
/// linting machine's filesystem layout into a SARIF file that CI typically uploads as a build
/// artifact.
fn relative_uri(path: &Path, cwd: Option<&Path>, roots: &[PathBuf]) -> String {
    if let Some(cwd) = cwd
        && let Ok(rel) = path.strip_prefix(cwd)
    {
        return forward_slashed(rel);
    }
    for root in roots {
        if let Ok(rel) = path.strip_prefix(root) {
            return forward_slashed(rel);
        }
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        // Last-resort fallback (a path with no file name, e.g. `/` or a `..`-tail — never a real
        // `.otui`/manifest target): strip any leading slash so the URI is *provably* relative and can
        // never leak an absolute machine path into a committed SARIF artifact.
        .unwrap_or_else(|| forward_slashed(path).trim_start_matches('/').to_owned())
}

/// Join `path`'s components with `/`, regardless of the host OS's native separator — SARIF's URIs
/// are always forward-slashed (RFC 3986), and this crate otherwise only ever runs on Unix-like
/// hosts in CI, but the conversion costs nothing to make explicit.
fn forward_slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Render `findings` as a single SARIF 2.1.0 log (one `run`, one `tool.driver`) whose
/// `results` and `rules` are derived entirely from `findings` — no fixed code list, so a future
/// diagnostic code is picked up automatically (mirrors `run_check`'s own `--deny` design).
///
/// `roots` are the workspace root(s) `check` discovered (see `discover_roots`), used only to
/// relativize each finding's `artifactLocation.uri`.
pub(crate) fn render(findings: &[Finding], roots: &[PathBuf]) -> Value {
    // Resolved once, not per finding: every finding in a single `check` invocation shares the
    // same process cwd (the repo checkout root a CI action runs `check` from — see
    // `relative_uri`'s doc comment for why it takes priority over a narrower discovered root).
    let cwd = std::env::current_dir().ok();

    // One `reportingDescriptor` per distinct code, in a stable (sorted) order so the SARIF log is
    // deterministic across runs of the same corpus — a `BTreeSet` dedupes and sorts in one step.
    let mut codes: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for finding in findings {
        codes.insert(finding.code.as_str());
    }
    let rules: Vec<Value> = codes
        .into_iter()
        .map(|code| {
            json!({
                "id": code,
                "shortDescription": { "text": code },
            })
        })
        .collect();

    let results: Vec<Value> = findings
        .iter()
        .map(|finding| {
            json!({
                "ruleId": finding.code,
                "level": severity_level(finding.severity),
                "message": { "text": finding.message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": relative_uri(&finding.path, cwd.as_deref(), roots) },
                        // SARIF regions are 1-based; `Finding::line`/`Finding::column_utf16` already
                        // are. `startColumn` MUST be `column_utf16`, not the byte `column`: SARIF
                        // 2.1.0's default `columnKind` is `utf16CodeUnits` (see `run.columnKind`
                        // below), and a byte column would misplace a result on any line with
                        // multibyte text before the finding's span.
                        "region": {
                            "startLine": finding.line,
                            "startColumn": finding.column_utf16,
                        }
                    }
                }]
            })
        })
        .collect();

    json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "otui-lsp",
                    "informationUri": "https://github.com/zoelner/otui-lsp",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules,
                }
            },
            // Explicit rather than relied-upon-by-default: `region.startColumn` above is always a
            // UTF-16 code-unit count (`Finding::column_utf16`), which happens to be SARIF 2.1.0's
            // default `columnKind` too — spelling it out here means a future column change can never
            // silently drift from what this `run` actually emits.
            "columnKind": "utf16CodeUnits",
            "results": results,
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One [`Finding`] of each [`Severity`], sharing a single distinct code per severity so the
    /// rule-dedup path (`codes`) is exercised alongside the level mapping.
    fn sample_findings(root: &Path) -> Vec<Finding> {
        vec![
            Finding {
                path: root.join("a.otui"),
                line: 2,
                column: 11,
                column_utf16: 11,
                severity: Severity::Error,
                code: "invalid-anchor-edge".to_owned(),
                message: "not a valid anchor edge".to_owned(),
            },
            Finding {
                path: root.join("b.otui"),
                line: 5,
                column: 3,
                column_utf16: 3,
                severity: Severity::Warning,
                code: "missing-asset".to_owned(),
                message: "asset not found".to_owned(),
            },
            Finding {
                path: root.join("c.otui"),
                line: 1,
                column: 1,
                column_utf16: 1,
                severity: Severity::Hint,
                code: "unknown-property".to_owned(),
                message: "unknown property".to_owned(),
            },
        ]
    }

    #[test]
    fn renders_a_valid_sarif_log_with_one_result_per_finding() {
        let root = PathBuf::from("/project/root");
        let findings = sample_findings(&root);

        let sarif = render(&findings, std::slice::from_ref(&root));

        assert_eq!(sarif["version"], "2.1.0");
        let runs = sarif["runs"].as_array().expect("runs is an array");
        assert_eq!(runs.len(), 1);
        let run = &runs[0];

        let results = run["results"].as_array().expect("results is an array");
        assert_eq!(results.len(), 3);

        // Severity -> level, exactly.
        assert_eq!(results[0]["level"], "error");
        assert_eq!(results[1]["level"], "warning");
        assert_eq!(results[2]["level"], "note");

        // ruleId matches the finding's code.
        assert_eq!(results[0]["ruleId"], "invalid-anchor-edge");
        assert_eq!(results[1]["ruleId"], "missing-asset");
        assert_eq!(results[2]["ruleId"], "unknown-property");

        // Region is 1-based and equal to the finding's own (already 1-based) line/column.
        let region0 = &results[0]["locations"][0]["physicalLocation"]["region"];
        assert_eq!(region0["startLine"], 2);
        assert_eq!(region0["startColumn"], 11);
        let region2 = &results[2]["locations"][0]["physicalLocation"]["region"];
        assert_eq!(region2["startLine"], 1);
        assert_eq!(region2["startColumn"], 1);

        // The URI is relative to `root`, forward-slashed, never absolute.
        let uri0 = results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
            .as_str()
            .expect("uri is a string");
        assert_eq!(uri0, "a.otui");
        assert!(!uri0.starts_with('/'));

        // One rule per distinct code, each carrying its `id`.
        let rules = run["tool"]["driver"]["rules"]
            .as_array()
            .expect("rules is an array");
        assert_eq!(rules.len(), 3);
        let rule_ids: std::collections::BTreeSet<&str> =
            rules.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(
            rule_ids,
            std::collections::BTreeSet::from([
                "invalid-anchor-edge",
                "missing-asset",
                "unknown-property",
            ])
        );

        assert_eq!(run["tool"]["driver"]["name"], "otui-lsp");
    }

    #[test]
    fn a_path_outside_every_root_falls_back_to_its_file_name() {
        let root = PathBuf::from("/project/root");
        let finding = Finding {
            path: PathBuf::from("/somewhere/else/loose.otui"),
            line: 1,
            column: 1,
            column_utf16: 1,
            severity: Severity::Error,
            code: "x".to_owned(),
            message: "m".to_owned(),
        };
        let sarif = render(std::slice::from_ref(&finding), std::slice::from_ref(&root));
        let uri =
            sarif["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["artifactLocation"]
                ["uri"]
                .as_str()
                .expect("uri is a string");
        // Never the machine's absolute path — either a cwd-relative path (if this test happens to
        // run under `/somewhere/else`, which it won't) or the bare file name.
        assert!(!uri.starts_with('/'), "uri must not be absolute: {uri}");
    }

    // --- mandatory revert experiment (documented in the node's report, not re-run here): mapping
    // every `Severity` to `"error"` in `severity_level` makes `renders_a_valid_sarif_log_with_one_result_per_finding`'s
    // `results[1]["level"] == "warning"` / `results[2]["level"] == "note"` assertions fail — i.e.
    // this test IS the guard for the severity->level map.

    /// Regression (CodeRabbit): `relative_uri` used to strip a discovered `root` prefix FIRST and
    /// fall back to `cwd` only when no root matched. GitHub code-scanning's `upload-sarif` action
    /// always maps `artifactLocation.uri` onto the repo tree relative to wherever the action
    /// runs — the process `cwd` — never relative to a narrower workspace root `check` happened to
    /// discover for one target. Here `cwd` is `/repo`, the discovered `root` is the narrower
    /// `/repo/ui`, and the finding sits at `/repo/ui/widget.otui`: the URI must be `ui/widget.otui`
    /// (relative to `cwd`), NOT `widget.otui` (relative to the narrower root).
    ///
    /// `relative_uri` takes its `cwd` as an explicit parameter rather than reading
    /// `std::env::current_dir()` directly, precisely so this scenario can be pinned deterministically
    /// without mutating the test process's real working directory (which `cargo test`'s parallel
    /// test threads share, making any such mutation racy).
    #[test]
    fn relative_uri_prefers_cwd_over_a_narrower_discovered_root() {
        let cwd = PathBuf::from("/repo");
        let root = PathBuf::from("/repo/ui");
        let path = PathBuf::from("/repo/ui/widget.otui");

        let uri = relative_uri(&path, Some(&cwd), std::slice::from_ref(&root));

        assert_eq!(uri, "ui/widget.otui");
    }

    /// With no `cwd` match (or no `cwd` at all — e.g. `std::env::current_dir()` failed), a
    /// discovered root is still used, preserving the pre-fix behavior for that case.
    #[test]
    fn relative_uri_falls_back_to_a_root_when_cwd_does_not_match() {
        let cwd = PathBuf::from("/somewhere/unrelated");
        let root = PathBuf::from("/repo/ui");
        let path = PathBuf::from("/repo/ui/widget.otui");

        assert_eq!(
            relative_uri(&path, Some(&cwd), std::slice::from_ref(&root)),
            "widget.otui"
        );
        assert_eq!(
            relative_uri(&path, None, std::slice::from_ref(&root)),
            "widget.otui"
        );
    }

    /// SARIF's default `columnKind` is `utf16CodeUnits`; `region.startColumn` must reflect that,
    /// not the byte column `Finding::column` carries for the human output.
    ///
    /// A `!text: '…'` value earlier on the line, containing two non-ASCII, 2-byte-but-1-UTF16-unit
    /// characters ("é" twice), pushes the finding's byte column two units ahead of its UTF-16
    /// column — a realistic shape (an accented localized string) that would misplace a GitHub
    /// annotation if the byte column leaked into SARIF.
    #[test]
    fn sarif_start_column_is_utf16_not_bytes_on_a_multibyte_line() {
        let root = PathBuf::from("/project/root");
        let finding = Finding {
            path: root.join("a.otui"),
            line: 1,
            // Byte column of the invalid property on a line like `  !text: 'café café', bogus: 1`:
            // two 2-byte 'é's before it inflate the byte column two past the UTF-16 column.
            column: 30,
            column_utf16: 28,
            severity: Severity::Hint,
            code: "unknown-property".to_owned(),
            message: "unknown property".to_owned(),
        };

        let sarif = render(std::slice::from_ref(&finding), std::slice::from_ref(&root));

        let region = &sarif["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"];
        assert_eq!(
            region["startColumn"], 28,
            "SARIF must use the UTF-16 column"
        );
        assert_ne!(
            region["startColumn"], finding.column,
            "the UTF-16 and byte columns must be demonstrably different on this fixture"
        );

        // `columnKind` is spelled out explicitly to match what `startColumn` actually is.
        assert_eq!(sarif["runs"][0]["columnKind"], "utf16CodeUnits");
    }
}
