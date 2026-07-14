//! End-to-end test of the `otui-lsp fmt` CLI subcommand: invokes the actual compiled binary (not
//! [`otui_lsp_server::cli::run_fmt`] directly) so the `argv[1]` dispatch in `main` is covered too,
//! not just the library function it delegates to.

use std::path::Path;
use std::process::Command;

/// A mis-indented but syntactically valid `.otui` fixture: `format` re-indents the property line
/// to 2 spaces, so `--check` must report it as needing formatting.
const MISINDENTED: &str = "MyWidget\n      id: foo\n";

/// Already in the canonical form `format` would produce for [`MISINDENTED`].
const CANONICAL: &str = "MyWidget\n  id: foo\n";

/// A scratch directory unique to this test process, under the OS temp dir. Removed at the end of
/// each test on a best-effort basis.
fn scratch_dir(case: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("otui-lsp-cli-e2e-{}-{case}", std::process::id()))
}

/// Run the compiled `otui-lsp` binary with `args`, returning `(exit code, stdout, stderr)`.
fn run_otui_lsp(args: &[&str], cwd: &Path) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_otui-lsp"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn otui-lsp binary");
    (
        output.status.code().expect("process exited with a code"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn fmt_check_reports_a_misindented_file_and_exits_nonzero() {
    let dir = scratch_dir("needs-formatting");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let file = dir.join("widget.otui");
    std::fs::write(&file, MISINDENTED).expect("write fixture");

    let (code, stdout, _stderr) = run_otui_lsp(&["fmt", "--check", dir.to_str().unwrap()], &dir);

    assert_ne!(code, 0, "a file needing formatting must exit non-zero");
    assert!(
        stdout.contains("widget.otui"),
        "stdout should name the offending file: {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fmt_check_with_a_relative_directory_argument_still_finds_files() {
    // Regression: `fmt --check .` — a RELATIVE directory, the primary CI invocation — must still
    // find the dirty files, not silently exit clean. `collect_files_under` keys by absolute
    // `file://` URL, so `resolve_targets` canonicalizes first; without that, a relative dir dropped
    // every file and produced a dangerous false-clean SUCCESS.
    let dir = scratch_dir("relative-dir");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("widget.otui"), MISINDENTED).expect("write fixture");

    // cwd is the fixture directory; the argument is the relative ".".
    let (code, stdout, _stderr) = run_otui_lsp(&["fmt", "--check", "."], &dir);

    assert_ne!(
        code, 0,
        "a relative-dir argument must still find the dirty file, not exit clean"
    );
    assert!(
        stdout.contains("widget.otui"),
        "stdout should name the offending file even for a relative dir: {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fmt_check_on_a_canonical_file_exits_zero() {
    let dir = scratch_dir("already-canonical");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let file = dir.join("widget.otui");
    std::fs::write(&file, CANONICAL).expect("write fixture");

    let (code, stdout, _stderr) = run_otui_lsp(&["fmt", "--check", dir.to_str().unwrap()], &dir);

    assert_eq!(code, 0, "an already-canonical file must exit clean");
    assert!(
        stdout.contains("0 file(s) need formatting"),
        "stdout should report zero files needing formatting: {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fmt_check_on_a_non_utf8_otui_file_under_the_target_dir_is_not_silently_skipped() {
    // Same regression as `cli_check.rs`'s equivalent test, for the `fmt --check` discovery path:
    // a non-UTF-8/binary `.otui` under a target directory must still be discovered as a target
    // (path-only `collect_paths_under`), so its `std::fs::read_to_string` failure is reported
    // instead of the file being silently absent from the walk.
    let dir = scratch_dir("binary-otui");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("widget.otui"), CANONICAL).expect("write good fixture");
    std::fs::write(dir.join("bad.otui"), [0xffu8, 0xfe, 0x00]).expect("write binary fixture");

    let (code, stdout, stderr) = run_otui_lsp(&["fmt", "--check", dir.to_str().unwrap()], &dir);

    assert_ne!(
        code, 0,
        "a non-UTF-8 .otui under the target dir must fail, not silently exit clean: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("bad.otui"),
        "stderr should name the unreadable file: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fmt_write_rewrites_the_file_in_place() {
    let dir = scratch_dir("write");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let file = dir.join("widget.otui");
    std::fs::write(&file, MISINDENTED).expect("write fixture");

    let (code, stdout, _stderr) = run_otui_lsp(&["fmt", "--write", dir.to_str().unwrap()], &dir);

    assert_eq!(code, 0, "--write always succeeds barring an I/O error");
    assert!(
        stdout.contains("widget.otui"),
        "stdout should name the formatted file: {stdout}"
    );
    let on_disk = std::fs::read_to_string(&file).expect("read back");
    assert_eq!(on_disk, CANONICAL);

    std::fs::remove_dir_all(&dir).ok();
}
