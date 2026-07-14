//! End-to-end test of the `otui-lsp check` CLI subcommand: invokes the actual compiled binary (not
//! [`otui_lsp_server::cli::run_check`] directly) so the `argv[1]` dispatch in `main` is covered
//! too, exactly like `tests/cli_fmt.rs` does for `fmt`.
//!
//! Every fixture here is a small **project directory** (an `.otmod` alongside the `.otui`/`.lua`
//! files), never a single loose file — see `an_unknown_property_only_lua_declared_widget_prop_is_not_flagged`'s
//! doc comment for why that project shape is itself part of what is under test.

use std::path::Path;
use std::process::Command;

/// A scratch directory unique to this test process + case name, under the OS temp dir. Removed at
/// the end of each test on a best-effort basis.
fn scratch_dir(case: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "otui-lsp-cli-check-e2e-{}-{case}",
        std::process::id()
    ))
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

/// A minimal `.otmod` manifest — just enough for the fixture directory to read as a real module
/// project, not a loose scratch file. `check` itself does not require one (see `discover_roots`'s
/// fallback), but every fixture here carries one anyway to mirror a real target.
const OTMOD: &str = "Module\n  name: fixture\n  description: fixture module for otui-lsp check\n";

#[test]
fn a_real_error_is_reported_and_exits_nonzero_by_default() {
    let dir = scratch_dir("invalid-anchor-edge");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("fixture.otmod"), OTMOD).expect("write otmod");
    // `lft` is not a valid anchor edge (spec §5.3's six edges) — a hard engine error, never a hint.
    // A top-level `Name < UIWidget` style header (not a bare container tag) so this is the only
    // finding — a bare root tag would additionally need to resolve as its own style
    // (`unknown-root-style`, an unrelated error this test does not care about).
    let otui = "MyWidget < UIWidget\n  anchors.lft: parent\n";
    let file = dir.join("widget.otui");
    std::fs::write(&file, otui).expect("write fixture");

    let (code, stdout, _stderr) = run_otui_lsp(&["check", dir.to_str().unwrap()], &dir);

    assert_ne!(
        code, 0,
        "an invalid-anchor-edge error must exit non-zero by default (--deny error)"
    );
    assert!(
        stdout.contains("error[invalid-anchor-edge]"),
        "stdout should print the error's code: {stdout}"
    );
    // Output-format assertion: `path:line:col:` shape, 1-based. `anchors.lft` sits on line 2
    // (1-based), and `lft` starts at column 11 (1-based byte column: two leading spaces + "anchors.").
    let expected_prefix = format!("{}:2:11:", file.display());
    assert!(
        stdout.contains(&expected_prefix),
        "stdout should carry a 1-based `path:line:col:` prefix ({expected_prefix}): {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unknown_property_alone_is_a_hint_and_exits_zero_by_default_but_nonzero_under_deny_hints() {
    let dir = scratch_dir("unknown-property");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("fixture.otmod"), OTMOD).expect("write otmod");
    // `widht` is a typo for `width` — unknown to the C++ catalog, but the engine silently ignores an
    // unknown ordinary property (spec §2.10: a hint, never an error or a warning). A top-level
    // `Name < UIWidget` style header (not a bare container tag) so the ONLY finding is the hint
    // under test — a bare root tag would additionally need to resolve as its own style
    // (`unknown-root-style`, an unrelated error this test does not care about).
    let otui = "MyPanel < UIWidget\n  widht: 10\n";
    std::fs::write(dir.join("widget.otui"), otui).expect("write fixture");

    let (code, stdout, _stderr) = run_otui_lsp(&["check", dir.to_str().unwrap()], &dir);
    assert_eq!(
        code, 0,
        "a hint-only finding must exit zero under the default --deny error: {stdout}"
    );
    assert!(
        stdout.contains("hint[unknown-property]"),
        "stdout should print the hint's code: {stdout}"
    );

    let (deny_hints_code, deny_hints_stdout, _stderr) =
        run_otui_lsp(&["check", dir.to_str().unwrap(), "--deny", "hints"], &dir);
    assert_ne!(
        deny_hints_code, 0,
        "the same hint-only finding must exit non-zero under --deny hints: {deny_hints_stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_lua_declared_widget_property_is_not_flagged_as_unknown_even_for_a_single_linted_file() {
    // The fidelity regression this whole node exists to prevent: linting `table.otui` in isolation
    // must still see `uitable.lua`'s custom `column-style` declaration, because `check` builds its
    // workspace indexes from the file's containing PROJECT (`build_indexes` over the discovered
    // root), not just the one file passed on the command line.
    let dir = scratch_dir("lua-widget-aware");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("fixture.otmod"), OTMOD).expect("write otmod");
    std::fs::write(
        dir.join("uitable.lua"),
        "\
UITable = extends(UIWidget, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'column-style' then
    end
  end
end
",
    )
    .expect("write lua");
    let otui_file = dir.join("table.otui");
    std::fs::write(&otui_file, "Table < UITable\n  column-style: SomeColumn\n")
        .expect("write fixture");

    // Lint ONLY the `.otui` file (never a directory), so the only way `column-style` could be
    // recognized is `check` having scanned the sibling `.lua` file via its discovered project root.
    let (code, stdout, _stderr) = run_otui_lsp(&["check", otui_file.to_str().unwrap()], &dir);

    assert_eq!(
        code, 0,
        "column-style must be accepted once uitable.lua's widget def is indexed: {stdout}"
    );
    assert!(
        !stdout.contains("unknown-property"),
        "column-style must never be reported unknown-property: {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_malformed_otmod_manifest_is_an_error() {
    // A `.otmod` with no top-level `Module` node fails to load as a module — `missing-module-root`
    // is an Error, so `check --deny error` (the default) must fail. Without manifest scanning the
    // CLI would silently pass this broken project (the CI footgun this closes).
    let dir = scratch_dir("bad-otmod");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("broken.otmod"), "SomethingElse\n  name: x\n").expect("write otmod");

    let (code, stdout, _stderr) = run_otui_lsp(&["check", dir.to_str().unwrap()], &dir);
    assert_ne!(
        code, 0,
        "a malformed .otmod (missing Module root) must fail --deny error: {stdout}"
    );
    assert!(
        stdout.contains("error[missing-module-root]"),
        "stdout should print the manifest error code: {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_manifest_file_passed_directly_is_linted_not_rejected() {
    // Regression: `check <file>.otmod` (a manifest passed directly, not via its directory) must be
    // linted, not rejected as "neither a directory nor a `.otui` file". USAGE advertises manifest
    // checking, so a direct manifest argument has to be accepted too.
    let dir = scratch_dir("direct-otmod");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let manifest = dir.join("broken.otmod");
    std::fs::write(&manifest, "SomethingElse\n  name: x\n").expect("write otmod");

    let (code, stdout, stderr) = run_otui_lsp(&["check", manifest.to_str().unwrap()], &dir);
    assert!(
        !stderr.contains("neither a directory"),
        "a direct manifest file must not be rejected: {stderr}"
    );
    assert_ne!(
        code, 0,
        "the malformed manifest is still an error: {stdout}"
    );
    assert!(
        stdout.contains("error[missing-module-root]"),
        "stdout should print the manifest error even for a direct file arg: {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_missing_asset_reference_is_a_warning() {
    // A broken `image-source:` is a `missing-asset` Warning the live server publishes; `check` must
    // see it too. It requires a detectable client root (init.lua + data/ + modules/), so the fixture
    // is a minimal client tree with the `.otui` under `modules/`.
    let root = scratch_dir("missing-asset");
    let module = root.join("modules").join("m");
    std::fs::create_dir_all(&module).expect("create module dir");
    std::fs::create_dir_all(root.join("data")).expect("create data dir");
    std::fs::write(root.join("init.lua"), "require('nothing')\n").expect("write init.lua");
    let otui_file = module.join("m.otui");
    std::fs::write(
        &otui_file,
        "Panel < UIWidget\n  image-source: does-not-exist.png\n",
    )
    .expect("write fixture");

    // Default (--deny error): the Warning does not fail the build.
    let (code, stdout, _stderr) = run_otui_lsp(&["check", otui_file.to_str().unwrap()], &root);
    assert_eq!(
        code, 0,
        "a missing-asset warning must NOT fail the default --deny error: {stdout}"
    );
    assert!(
        stdout.contains("warning[missing-asset]"),
        "stdout should report the broken asset reference: {stdout}"
    );

    // --deny warnings: now it fails.
    let (deny_code, deny_stdout, _stderr) = run_otui_lsp(
        &["check", otui_file.to_str().unwrap(), "--deny", "warnings"],
        &root,
    );
    assert_ne!(
        deny_code, 0,
        "the same missing-asset warning must fail under --deny warnings: {deny_stdout}"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_non_utf8_otui_file_under_the_target_directory_is_not_silently_skipped() {
    // Regression (CodeRabbit): directory expansion used to key discovered files by a successful
    // content read (`collect_files_under`/`read_indexed_file`), so an unreadable, oversized, or
    // non-UTF-8/binary `.otui` under a target directory was silently absent from the target list —
    // `check` never looked at it and could exit SUCCESS over a broken file. Discovery must be
    // path-only (`collect_paths_under`): the file is still found as a target, and the per-target
    // `std::fs::read_to_string` then reports (not skips) the failure.
    let dir = scratch_dir("binary-otui");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("fixture.otmod"), OTMOD).expect("write otmod");
    std::fs::write(dir.join("widget.otui"), "MyPanel < UIWidget\n  width: 10\n")
        .expect("write good fixture");
    // Invalid UTF-8 (a lone continuation byte pair) named with the `.otui` extension.
    std::fs::write(dir.join("bad.otui"), [0xffu8, 0xfe, 0x00]).expect("write binary fixture");

    let (code, stdout, stderr) = run_otui_lsp(&["check", dir.to_str().unwrap()], &dir);

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
#[cfg(unix)]
fn an_unreadable_subdirectory_fails_check_instead_of_being_silently_omitted() {
    // Regression (CodeRabbit): `collect_paths_under`'s `std::fs::read_dir` `Ok`-guard turned an
    // unreadable directory into an empty subtree, so `check <dir>` could silently omit a whole
    // unreadable subdirectory's `.otui` files and still exit SUCCESS. It must instead propagate the
    // error and fail, naming the unreadable directory.
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("unreadable-subdir");
    let locked = dir.join("locked");
    std::fs::create_dir_all(&locked).expect("create scratch dirs");
    std::fs::write(dir.join("fixture.otmod"), OTMOD).expect("write otmod");
    std::fs::write(dir.join("widget.otui"), "MyPanel < UIWidget\n  width: 10\n")
        .expect("write good fixture");
    // A file under `locked/` that would be found if the directory were readable — proves the walk
    // really would have looked inside it, not that it happened to be empty anyway.
    std::fs::write(
        locked.join("hidden.otui"),
        "Hidden < UIWidget\n  width: 10\n",
    )
    .expect("write fixture under locked dir");

    let original_perms = std::fs::metadata(&locked)
        .expect("stat locked dir")
        .permissions();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("lock down permissions");

    let (code, stdout, stderr) = run_otui_lsp(&["check", dir.to_str().unwrap()], &dir);

    // Restore permissions immediately so cleanup (`remove_dir_all`) below can actually work,
    // regardless of what the assertions below find.
    std::fs::set_permissions(&locked, original_perms).ok();

    assert_ne!(
        code, 0,
        "an unreadable subdirectory must fail check, not silently exit clean: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("locked"),
        "stderr should name the unreadable directory: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_uppercase_extension_is_discovered_case_insensitively() {
    // Regression (CodeRabbit): `collect_paths_under` compared the extension with `==`, so
    // `Widget.OTUI` was silently skipped even though `Language::from_uri` recognizes an uppercase
    // extension. It must be discovered (and checked) just like a lowercase `.otui` file.
    let dir = scratch_dir("uppercase-ext");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("fixture.otmod"), OTMOD).expect("write otmod");
    // `lft` is not a valid anchor edge — a hard engine error, so if the uppercase file is
    // discovered, `check` must report it and exit non-zero.
    std::fs::write(
        dir.join("Widget.OTUI"),
        "MyWidget < UIWidget\n  anchors.lft: parent\n",
    )
    .expect("write uppercase-extension fixture");

    let (code, stdout, _stderr) = run_otui_lsp(&["check", dir.to_str().unwrap()], &dir);

    assert_ne!(
        code, 0,
        "the uppercase-extension file must be discovered and checked: {stdout}"
    );
    assert!(
        stdout.contains("Widget.OTUI"),
        "stdout should name the uppercase-extension file: {stdout}"
    );
    assert!(
        stdout.contains("error[invalid-anchor-edge]"),
        "stdout should report the anchor error found inside it: {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn summary_line_and_clean_project_exit_zero() {
    let dir = scratch_dir("clean");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    std::fs::write(dir.join("fixture.otmod"), OTMOD).expect("write otmod");
    std::fs::write(dir.join("widget.otui"), "MyPanel < UIWidget\n  width: 10\n")
        .expect("write fixture");

    let (code, stdout, _stderr) = run_otui_lsp(&["check", dir.to_str().unwrap()], &dir);
    assert_eq!(code, 0, "a clean project must exit zero: {stdout}");
    assert!(
        stdout.contains("0 error(s), 0 warning(s), 0 hint(s)"),
        "stdout should print a zeroed summary line: {stdout}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
