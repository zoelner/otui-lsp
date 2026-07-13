//! End-to-end transport test: drive the real handshake + dispatch loop over an in-memory
//! [`lsp_server::Connection`] (no stdio), proving `initialize → didOpen → hover → shutdown/exit`
//! works through `Backend::handle_request`/`handle_notification`.

use std::path::Path;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    DiagnosticSeverity, DidOpenTextDocumentParams, HoverParams, InitializeParams,
    InitializedParams, InlayHintParams, NumberOrString, Position, PublishDiagnosticsParams,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    WorkDoneProgressParams, WorkspaceFolder,
};
use otui_lsp_server::{serve, Backend, Termination};

/// Build a `file:` [`Uri`] from a filesystem path via the `url` crate's percent-encoding — never by
/// hand-formatting `format!("file://{}", path.display())`, which leaves reserved characters (a
/// space, `#`, `?`, …) unencoded and produces an invalid/misinterpreted URI. Mirrors the server's own
/// `uri_from_file_path` (private to the crate, so this is a test-local equivalent, not a second
/// implementation the server itself relies on).
fn file_uri(path: &Path) -> Uri {
    Uri::from_str(
        url::Url::from_file_path(path)
            .expect("valid file path")
            .as_str(),
    )
    .expect("uri")
}

/// The zero-based LSP [`Position`] of the first occurrence of `needle` in `text`. Test-only, and
/// deliberately simple: every text passed to it here is ASCII, so a UTF-8 byte offset and a UTF-16
/// code-unit column coincide.
fn position_of(text: &str, needle: &str) -> Position {
    let idx = text.find(needle).expect("needle present in text");
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (i, ch) in text[..idx].char_indices() {
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    Position::new(line, (idx - line_start) as u32)
}

/// RAII guard that removes its directory (recursively) on drop — including on an unwinding panic
/// from a failed assertion, unlike a trailing `std::fs::remove_dir_all` call, which is never reached
/// once an earlier assertion panics and leaks the temp directory.
struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How long [`recv_response`]/[`recv_diagnostics`] wait for the expected message before giving up.
/// Generous for a fully in-memory, single-process test (no real network), but bounded: a blocking
/// `recv()` here would hang the *whole test binary* — every test after it too, since `cargo test`
/// only reports a suite-wide timeout, never which test stalled — if publication ever regresses or
/// the server thread dies without sending what the test is waiting for (CodeRabbit Finding 5 on
/// PR #51).
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// Read from the client end until the [`Response`] for `id` arrives, skipping anything else the
/// server pushed in the meantime (log notifications, `client/registerCapability` requests, …).
///
/// Bounded by [`RECV_TIMEOUT`] (see its doc comment for why a blocking `recv()` here is not safe).
fn recv_response(client: &Connection, id: &RequestId) -> Response {
    loop {
        match client.receiver.recv_timeout(RECV_TIMEOUT) {
            Ok(Message::Response(resp)) if &resp.id == id => return resp,
            Ok(_) => continue,
            Err(e) => panic!(
                "timed out after {RECV_TIMEOUT:?} waiting for a response to request {id:?} \
                 (server channel: {e})"
            ),
        }
    }
}

/// The server side, mirroring the binary's `main`: handshake, drive post-init once, then run the
/// shared [`serve`] receive loop. Returns how the loop terminated so the test can assert the exit
/// classification (clean shutdown vs. standalone exit).
fn run_server(server: Connection) -> Termination {
    let (id, params) = server.initialize_start().expect("initialize_start");
    let init_params: InitializeParams =
        serde_json::from_value(params).expect("deserialize InitializeParams");
    let backend = Backend::new(server.sender.clone(), &init_params);
    let result = serde_json::to_value(backend.initialize_result()).expect("serialize result");
    server
        .initialize_finish(id, result)
        .expect("initialize_finish");

    // `initialize_finish` consumed the `initialized` notification; drive post-init work once.
    backend.handle_notification(Notification {
        method: "initialized".to_owned(),
        params: serde_json::Value::Null,
    });

    serve(&backend, &server).expect("serve loop")
}

/// Read from the client end until a `textDocument/publishDiagnostics` notification for `uri`
/// arrives, skipping anything else in between (log notifications, `client/registerCapability`
/// requests, a diagnostics push for some other document, …).
///
/// Bounded by [`RECV_TIMEOUT`] (see its doc comment): a blocking `recv()` here would hang the whole
/// suite the moment `publishDiagnostics` ever regressed or the server thread died mid-test, instead
/// of failing just this one test with a readable message (CodeRabbit Finding 5 on PR #51).
fn recv_diagnostics(client: &Connection, uri: &Uri) -> PublishDiagnosticsParams {
    loop {
        match client.receiver.recv_timeout(RECV_TIMEOUT) {
            Ok(Message::Notification(n)) if n.method == "textDocument/publishDiagnostics" => {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(n.params).expect("deserialize PublishDiagnosticsParams");
                if &params.uri == uri {
                    return params;
                }
            }
            Ok(_) => continue,
            Err(e) => panic!(
                "timed out after {RECV_TIMEOUT:?} waiting for publishDiagnostics for {uri:?} \
                 (server channel: {e})"
            ),
        }
    }
}

/// Drive the client half of the handshake: `initialize` request/response, then `initialized`.
fn client_handshake(client: &Connection) {
    client_handshake_with_params(client, InitializeParams::default());
}

/// Like [`client_handshake`], but with caller-supplied `InitializeParams` — for tests that need a
/// real workspace root (e.g. so `/`-rooted asset paths have a data root to resolve against).
fn client_handshake_with_params(client: &Connection, params: InitializeParams) {
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(1),
            "initialize".to_owned(),
            params,
        )))
        .expect("send initialize");
    let init_resp = recv_response(client, &RequestId::from(1));
    assert!(
        init_resp.error.is_none(),
        "initialize errored: {init_resp:?}"
    );
    client
        .sender
        .send(Message::Notification(Notification::new(
            "initialized".to_owned(),
            InitializedParams {},
        )))
        .expect("send initialized");
}

#[test]
fn memory_connection_drives_initialize_open_hover_shutdown() {
    let (server, client) = Connection::memory();

    // The server side mirrors the binary's `main`: handshake, then the shared `serve` receive loop.
    let server_thread = thread::spawn(move || run_server(server));

    // 1. initialize → expect an InitializeResult carrying our capabilities.
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(1),
            "initialize".to_owned(),
            InitializeParams::default(),
        )))
        .expect("send initialize");
    let init_resp = recv_response(&client, &RequestId::from(1));
    assert!(
        init_resp.error.is_none(),
        "initialize errored: {init_resp:?}"
    );
    let init_value = init_resp.result.expect("initialize result present");
    assert!(
        init_value
            .get("capabilities")
            .and_then(|c| c.get("hoverProvider"))
            .is_some(),
        "capabilities must advertise a hover provider: {init_value}"
    );

    // Complete the handshake: the server's `initialize_finish` is blocking on this notification.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "initialized".to_owned(),
            InitializedParams {},
        )))
        .expect("send initialized");

    // 2. didOpen a small `.otui` document.
    let uri = Uri::from_str("file:///scratch/widget.otui").expect("uri");
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: "Child < UIWidget\n".to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    // 3. hover over the `UIWidget` base (line 0, char 8) → a plausible non-null response.
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position::new(0, 8),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let hover_value = hover_resp.result.expect("hover result present");
    assert!(
        !hover_value.is_null(),
        "hover over a native base should yield contents, got null"
    );

    // 4. shutdown + exit: the server answers shutdown, then the loop breaks on exit.
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(3),
            "shutdown".to_owned(),
            serde_json::Value::Null,
        )))
        .expect("send shutdown");
    let shutdown_resp = recv_response(&client, &RequestId::from(3));
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown errored: {shutdown_resp:?}"
    );
    client
        .sender
        .send(Message::Notification(Notification::new(
            "exit".to_owned(),
            serde_json::Value::Null,
        )))
        .expect("send exit");

    // The clean `shutdown` → `exit` handshake terminates with status 0.
    let termination = server_thread.join().expect("server thread joined");
    assert_eq!(termination, Termination::Shutdown);
    assert_eq!(termination.exit_code(), 0);
}

/// A standalone `exit` notification (no preceding `shutdown`) must terminate the loop and be
/// classified as an abnormal exit (process status 1), never silently dropped.
#[test]
fn standalone_exit_terminates_with_nonzero_status() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    client_handshake(&client);

    // No `shutdown` first — send a bare `exit`. `serve` must stop and report an abnormal exit.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "exit".to_owned(),
            serde_json::Value::Null,
        )))
        .expect("send exit");

    let termination = server_thread.join().expect("server thread joined");
    assert_eq!(termination, Termination::Aborted);
    assert_eq!(termination.exit_code(), 1);
}

/// Send `shutdown` (request id `id`), wait for its response, then send a bare `exit` and join the
/// server thread — the closing dance every test below shares once its own assertions are done.
fn shutdown_and_exit(client: &Connection, server_thread: thread::JoinHandle<Termination>, id: i32) {
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(id),
            "shutdown".to_owned(),
            serde_json::Value::Null,
        )))
        .expect("send shutdown");
    let _ = recv_response(client, &RequestId::from(id));
    client
        .sender
        .send(Message::Notification(Notification::new(
            "exit".to_owned(),
            serde_json::Value::Null,
        )))
        .expect("send exit");
    server_thread.join().expect("server thread joined");
}

/// Mark `dir` as a detected OTClient install root (see `find_client_root`/`CLIENT_ROOT_MARKERS`):
/// an `init.lua` file plus `data/` and `modules/` subdirectories. Every `missing-asset` test below
/// needs this — without a detected client root the diagnostic is silent by design (Finding 2), so a
/// bare temp directory with no such markers is no longer enough to exercise the rule.
fn mark_as_client_root(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir client root");
    std::fs::write(dir.join("init.lua"), b"-- stand-in for the real init.lua\n").expect("init.lua");
    std::fs::create_dir_all(dir.join("data")).expect("mkdir data");
    std::fs::create_dir_all(dir.join("modules")).expect("mkdir modules");
}

/// `missing-asset` end-to-end, over real files: a `.png` that exists on disk must stay silent, and
/// one that does not must produce exactly one Warning pointing at the offending path.
///
/// This drives the whole seam — workspace root capture at `initialize`, client-root detection, the
/// document's own directory, `resolve_asset_candidates`' probe variants, the `.is_file()` check —
/// because that is where the rule can actually break. A test of the pure part would prove nothing
/// about the disk.
#[test]
fn missing_asset_diagnostic_fires_only_for_the_absent_file() {
    let base = std::env::temp_dir().join(format!("otui-missing-asset-{}", std::process::id()));
    let _cleanup = TempDirGuard(base.clone());
    let images = base.join("images");
    std::fs::create_dir_all(&images).expect("mkdir");
    // The asset that exists. `resolve_asset_candidates` probes the `.png` form of an extensionless
    // path, so `/images/present` must find this file and stay quiet.
    std::fs::write(images.join("present.png"), b"png").expect("write asset");
    // The workspace root must be a *detected* OTClient install root (Finding 2) — otherwise a
    // `/`-rooted path has no data root the rule trusts, and nothing is diagnosed at all.
    mark_as_client_root(&base);

    let doc_path = base.join("widget.otui");
    let source = "\
Panel < UIWidget
  image-source: /images/present
  icon: /images/absent
";
    std::fs::write(&doc_path, source).expect("write doc");

    let root = file_uri(&base);
    let uri = file_uri(&doc_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    #[allow(deprecated)]
    client_handshake_with_params(
        &client,
        InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: "ws".to_owned(),
            }]),
            ..InitializeParams::default()
        },
    );

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let diags = recv_diagnostics(&client, &uri);
    let missing: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(NumberOrString::String("missing-asset".to_owned())))
        .collect();

    assert_eq!(
        missing.len(),
        1,
        "exactly one asset is absent; got {missing:#?}"
    );
    let d = missing[0];
    assert_eq!(d.severity, Some(DiagnosticSeverity::WARNING));
    assert!(
        d.message.contains("/images/absent"),
        "message must name the unresolved path: {}",
        d.message
    );
    // Line 2 (0-based) is the `icon:` line — the Warning sits on the value, not the whole document.
    assert_eq!(
        d.range.start.line, 2,
        "range must point at the `icon:` line"
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// Finding 1 on PR #51, pinned end-to-end: a workspace holding **two** unrelated OTClient install
/// roots (two client checkouts opened as separate workspace folders — not contrived; e.g. comparing
/// a fork against upstream) must resolve each document against **its own** root only. An asset that
/// exists only under the *other* root must never rescue a `missing-asset` finding for a document that
/// has nothing to do with that other install.
#[test]
fn missing_asset_diagnostic_is_not_rescued_by_an_unrelated_second_client_root() {
    let base = std::env::temp_dir().join(format!(
        "otui-two-client-roots-e2e-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());
    let root_a = base.join("client-a");
    let root_b = base.join("client-b");
    mark_as_client_root(&root_a);
    mark_as_client_root(&root_b);
    // The asset exists only under root B's `data/` overlay — never under root A.
    let images_b = root_b.join("data").join("images");
    std::fs::create_dir_all(&images_b).expect("mkdir");
    std::fs::write(images_b.join("shared.png"), b"png").expect("write asset");

    let doc_path = root_a.join("widget.otui");
    let source = "\
Panel < UIWidget
  icon: /images/shared
";
    std::fs::write(&doc_path, source).expect("write doc");

    let uri = file_uri(&doc_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    // Both roots opened as workspace folders — root A (the document's own tree) first, root B
    // second, so an implementation that naively concatenated every workspace root's client root
    // (the pre-fix behavior) would have found the asset via B and wrongly stayed silent.
    #[allow(deprecated)]
    client_handshake_with_params(
        &client,
        InitializeParams {
            workspace_folders: Some(vec![
                WorkspaceFolder {
                    uri: file_uri(&root_a),
                    name: "client-a".to_owned(),
                },
                WorkspaceFolder {
                    uri: file_uri(&root_b),
                    name: "client-b".to_owned(),
                },
            ]),
            ..InitializeParams::default()
        },
    );

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let diags = recv_diagnostics(&client, &uri);
    let missing: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(NumberOrString::String("missing-asset".to_owned())))
        .collect();
    assert_eq!(
        missing.len(),
        1,
        "root B's asset must not rescue a document that belongs to root A — got {missing:#?}"
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// Pinned end-to-end, corrected after verifying against the real engine: a CodeRabbit review of this
/// crate (PR #51, Finding 2) claimed `init.lua` mounts only the overlay directories (`data/`,
/// `modules/`, `mods/`) and never the install root itself, and asked for a test proving a file at
/// `<installroot>/foo.png` must NOT satisfy `/foo.png`. That claim does not hold: `main.cpp`
/// unconditionally calls `g_resources.discoverWorkDir("init.lua")` before any Lua runs;
/// `ResourceManager::discoverWorkDir` (`resourcemanager.cpp`) mounts the install root via
/// `PHYSFS_mount` and — on the candidate directory that has `init.lua` — breaks out of its loop
/// *without* ever unmounting it, so the bare install root stays mounted for the whole session. This
/// pins the corrected, verified behavior instead: a file sitting directly at the install root DOES
/// satisfy a `/`-rooted reference, so `missing-asset` must stay silent for it. Real, shipped,
/// autoloaded OTClient modules depend on exactly this (see `otui_core::links::ASSET_MOUNT_DIRS`'s
/// doc comment for the on-disk corpus evidence).
#[test]
fn missing_asset_diagnostic_is_silent_for_a_file_sitting_directly_at_the_install_root() {
    let base = std::env::temp_dir().join(format!(
        "otui-root-itself-is-mounted-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());
    mark_as_client_root(&base);
    // Present directly at the install root, not under `mods/`/`modules/`/`data/` — exactly the
    // shape `discoverWorkDir`'s always-on mount serves.
    std::fs::write(base.join("foo.png"), b"png").expect("write asset");

    let doc_path = base.join("widget.otui");
    let source = "\
Panel < UIWidget
  icon: /foo
";
    std::fs::write(&doc_path, source).expect("write doc");

    let root = file_uri(&base);
    let uri = file_uri(&doc_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    #[allow(deprecated)]
    client_handshake_with_params(
        &client,
        InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: "ws".to_owned(),
            }]),
            ..InitializeParams::default()
        },
    );

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let diags = recv_diagnostics(&client, &uri);
    let missing: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(NumberOrString::String("missing-asset".to_owned())))
        .collect();
    assert!(
        missing.is_empty(),
        "a file at the bare install root must satisfy a `/`-rooted path — got {missing:#?}"
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// Finding 2, pinned end-to-end: a workspace root that is a **standalone module directory** — no
/// `init.lua`/`data/`/`modules/` anywhere above it, exactly the shape of a module or mod repository
/// opened on its own (the ordinary unit of distribution, and what the separate VS Code extension
/// will typically be pointed at) — must produce **zero** `missing-asset` diagnostics, even though
/// the document references an asset that is genuinely absent from disk. The old behavior (joining a
/// `/`-rooted path directly onto whatever the editor happened to open) would have flagged this; the
/// fix requires a *detected* client root before claiming anything is missing.
#[test]
fn missing_asset_diagnostic_is_silent_in_a_standalone_module_workspace() {
    let base = std::env::temp_dir().join(format!(
        "otui-standalone-module-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());
    // A module's own directory, opened as the workspace root — no `init.lua`, no sibling `data/` or
    // `modules/` anywhere above it. Deliberately does NOT call `mark_as_client_root`.
    let module_dir = base.join("client_topmenu");
    std::fs::create_dir_all(&module_dir).expect("mkdir");

    let doc_path = module_dir.join("topmenu.otui");
    let source = "\
TopMenu < UIWidget
  image-source: /images/topbuttons/audio
";
    std::fs::write(&doc_path, source).expect("write doc");

    let root = file_uri(&module_dir);
    let uri = file_uri(&doc_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    #[allow(deprecated)]
    client_handshake_with_params(
        &client,
        InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: "ws".to_owned(),
            }]),
            ..InitializeParams::default()
        },
    );

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let diags = recv_diagnostics(&client, &uri);
    let missing: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(NumberOrString::String("missing-asset".to_owned())))
        .collect();
    assert!(
        missing.is_empty(),
        "no client root is reachable from a standalone module workspace, so nothing may be \
         claimed missing — got {missing:#?}"
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// Finding 3, end-to-end: with a `*.otpkg` archive mounted anywhere under the detected client root,
/// `missing-asset` must stay silent workspace-wide — the engine resolves file existence through
/// `PHYSFS_exists` over every mounted archive, never a raw OS `is_file()`, so an asset shipped inside
/// the package is invisible to our probe and must not be flagged as broken.
#[test]
fn missing_asset_diagnostic_is_silent_when_an_otpkg_archive_is_mounted() {
    let base = std::env::temp_dir().join(format!(
        "otui-otpkg-suppression-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());
    mark_as_client_root(&base);
    // The mounted archive; its contents are never inspected (out of scope — see
    // `otpkg_present_under`'s doc comment), only its presence.
    let mods = base.join("mods");
    std::fs::create_dir_all(&mods).expect("mkdir mods");
    std::fs::write(mods.join("bundle.otpkg"), b"not a real zip").expect("write otpkg");

    let doc_path = base.join("widget.otui");
    let source = "\
Panel < UIWidget
  icon: /images/definitely-missing
";
    std::fs::write(&doc_path, source).expect("write doc");

    let root = file_uri(&base);
    let uri = file_uri(&doc_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    #[allow(deprecated)]
    client_handshake_with_params(
        &client,
        InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root,
                name: "ws".to_owned(),
            }]),
            ..InitializeParams::default()
        },
    );

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let diags = recv_diagnostics(&client, &uri);
    let missing: Vec<_> = diags
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(NumberOrString::String("missing-asset".to_owned())))
        .collect();
    assert!(
        missing.is_empty(),
        "a mounted .otpkg suppresses missing-asset workspace-wide — got {missing:#?}"
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// Hover Blocker 1, end-to-end: a resolved asset file whose name contains a literal `)` must not
/// truncate the hover's Markdown image destination. `url::Url::from_file_path` does not
/// percent-encode `(`/`)` (verified independently — they are RFC 3986 sub-delims, outside the
/// WHATWG path percent-encode set), so a raw `![](file:///…)` destination closes early at the first
/// `)` — this is the common case too: any workspace living under a directory like
/// `Program Files (x86)` would break on every asset hover, not just a deliberately hostile filename.
#[test]
fn hover_sprite_preview_wraps_a_path_containing_parentheses_in_angle_brackets() {
    let base = std::env::temp_dir().join(format!(
        "otui-hover-parens-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());
    std::fs::create_dir_all(&base).expect("mkdir");
    // The asset's own filename carries the `)` — the exact shape that closes an unwrapped Markdown
    // image destination early.
    let asset_path = base.join("evil).png");
    std::fs::write(&asset_path, b"png").expect("write asset");

    let doc_path = base.join("widget.otui");
    let source = "Panel < UIWidget\n  image-source: evil).png\n";
    std::fs::write(&doc_path, source).expect("write doc");

    let uri = file_uri(&doc_path);
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");
    // Drain the diagnostics push before issuing the hover request.
    let _ = recv_diagnostics(&client, &uri);

    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: position_of(source, "evil).png"),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    let expected_target = url::Url::from_file_path(&asset_path)
        .expect("valid file path")
        .to_string();
    let expected_image_line = format!("![](<{expected_target}>)");
    assert!(
        value.contains(&expected_image_line),
        "image destination must be angle-bracket-wrapped and unbroken by the `)`: {value:?}"
    );
    // The failure mode this test exists to catch: an *unwrapped* destination that closes at `evil`,
    // leaking `.png)` as trailing literal text right after it.
    assert!(
        !value.contains("![](file://"),
        "the image destination must never be emitted unwrapped: {value:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hover Blocker 2, end-to-end: a backtick inside the raw path *value* text (fully attacker-
/// controlled document content — no asset on disk, no workspace root needed) must not close the
/// hover's Markdown code span early and let the remainder render as live Markdown/HTML.
#[test]
fn hover_sprite_preview_fences_a_backtick_in_the_path_value() {
    let uri = Uri::from_str("file:///scratch/backtick.otui").expect("uri");
    // A single backtick would close a naive `` `{value}` `` span right after `x`, letting
    // `<b>BOLD</b> [click](https://evil.example)` render as live content.
    let source =
        "Panel < UIWidget\n  image-source: x` <b>BOLD</b> [click](https://evil.example) `y\n";

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");
    let _ = recv_diagnostics(&client, &uri);

    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: position_of(source, "x` <b>"),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    // The full hostile text must appear *inside* a code span, not escape into live Markdown: the
    // fence must be strictly longer than any backtick run the value itself carries (here, one), so
    // count how many consecutive backticks open the code span right after "**Asset** " and confirm
    // it is more than the one embedded in the value.
    let after_label = value
        .split_once("**Asset** ")
        .map(|(_, rest)| rest)
        .unwrap_or(&value);
    let fence_len = after_label.chars().take_while(|&c| c == '`').count();
    assert!(
        fence_len >= 2,
        "the fence must be longer than the value's own single backtick, got {fence_len} in {value:?}"
    );
    // The whole hostile payload must appear literally inside the span — proof the fence did not
    // simply drop content — and it must not appear a second time outside the span (which would mean
    // it also escaped).
    let occurrences = value.matches("[click](https://evil.example)").count();
    assert_eq!(
        occurrences, 1,
        "the payload must appear exactly once, fenced inside the code span: {value:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hover Blocker 2, blank-line variant: a Markdown code span cannot contain a blank line — the fence
/// is left open and everything after the blank line renders as a live paragraph. Backtick fencing
/// does not close this; only flattening the value to a single line does. A block-scalar value (`|`)
/// is how a blank line reaches `path_ref.path` from attacker-controlled document content, and the
/// cursor must be in the block *body* (not the `|` header line) for the value to be read.
#[test]
fn hover_sprite_preview_flattens_a_blank_line_in_a_block_scalar_path_value() {
    let uri = Uri::from_str("file:///scratch/blankline.otui").expect("uri");
    // The `|` block value carries its indented body — including the blank line — into the raw path
    // text. Without flattening, the hover markdown would contain `\n\n`, orphaning the code fence and
    // rendering `<b>PWN</b> [click](https://evil.example)` as a live paragraph.
    let source =
        "Panel < UIWidget\n  image-source: |\n    x\n\n    <b>PWN</b> [click](https://evil.example)\n";

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");
    let _ = recv_diagnostics(&client, &uri);

    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    // Cursor in the block body, where `asset_ref_at` reads the multi-line value.
                    position: position_of(source, "    x"),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    // The payload must be present (proof the value was read, not a vacuous pass) and the whole hover
    // must stay on a single paragraph: a blank line means the fence broke and the tail escaped into
    // live Markdown. This is the load-bearing assertion.
    assert!(
        value.contains("PWN"),
        "the block-scalar value must reach the hover: {value:?}"
    );
    assert!(
        !value.contains("\n\n"),
        "a blank line in the fenced value orphans the code span: {value:?}"
    );
    assert_eq!(
        value.matches("[click](https://evil.example)").count(),
        1,
        "the payload must appear exactly once, fenced, not escaped: {value:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// The Markdown string of a hover [`Response`]'s `contents.value` (panics on any other shape —
/// every hover the server emits is `MarkupContent`).
fn hover_markdown(resp: &Response) -> String {
    let result = resp.result.clone().expect("hover result present");
    result
        .get("contents")
        .and_then(|c| c.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("expected markup hover contents, got {result}"))
        .to_owned()
}

/// The end-of-token [`Position`] of `needle` on line `line` (0-based) of `text` — ASCII-only test
/// helper, distinct from [`position_of`] (which finds the *start* of the first whole-document
/// occurrence): several fixtures below use base names that are themselves substrings of other
/// tokens on the same line (e.g. `UIWidget` contains `Widget`), so this scopes the search to one
/// line and returns the position right after the match.
fn base_end_position(text: &str, line: u32, needle: &str) -> Position {
    let line_text = text
        .lines()
        .nth(line as usize)
        .unwrap_or_else(|| panic!("line {line} exists in {text:?}"));
    let col = line_text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} present on line {line}: {line_text:?}"))
        + needle.len();
    Position::new(line, col as u32)
}

/// `textDocument/codeLens`, end-to-end: a style with direct derivations gets exactly one lens on
/// its declared name, carrying the exact derived count; a style with none gets no lens at all.
#[test]
fn code_lens_reports_the_derived_count_on_the_style_name() {
    let uri = Uri::from_str("file:///scratch/lens.otui").expect("uri");
    // `Widget` has two direct derivations (`Foo`, `Bar`); neither of those has any derivation of
    // its own, so only `Widget` should get a lens.
    let source = "Widget < UIWidget\nFoo < Widget\nBar < Widget\n";

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");
    let _ = recv_diagnostics(&client, &uri);

    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/codeLens".to_owned(),
            lsp_types::CodeLensParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            },
        )))
        .expect("send codeLens");
    let resp = recv_response(&client, &RequestId::from(2));
    assert!(resp.error.is_none(), "codeLens errored: {resp:?}");
    let lenses: Vec<lsp_types::CodeLens> =
        serde_json::from_value(resp.result.expect("codeLens result present"))
            .expect("deserialize Vec<CodeLens>");

    assert_eq!(
        lenses.len(),
        1,
        "only Widget (which has derivations) should get a lens: {lenses:#?}"
    );
    let lens = &lenses[0];
    // The lens is anchored on the declared name ("Widget", columns 0..6 on line 0).
    assert_eq!(
        lens.range,
        lsp_types::Range::new(Position::new(0, 0), Position::new(0, 6))
    );
    let command = lens.command.as_ref().expect("lens carries a command");
    assert!(
        command.title.contains('2'),
        "title must report the exact derived count: {:?}",
        command.title
    );
    // The command id is handled by the companion VS Code extension: see `Backend::code_lens`'s
    // doc comment for why this isn't the built-in `editor.action.showReferences`. Pinned here so
    // an accidental rename/regression back to an empty id fails a test instead of shipping
    // silently.
    assert_eq!(
        command.command, "otui.showSubtypes",
        "the lens command must be the namespaced id the extension registers"
    );
    let arguments = command
        .arguments
        .as_ref()
        .expect("otui.showSubtypes carries [uri, position] arguments");
    assert_eq!(
        *arguments,
        vec![
            serde_json::to_value(&uri).expect("Uri serializes"),
            serde_json::to_value(Position::new(0, 0)).expect("Position serializes"),
        ],
        "arguments must be the style declaration's document URI and the lens position"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// `textDocument/inlayHint`, end-to-end: a based style whose resolved native ancestor differs from
/// the literal base token gets a `→ Native` hint right after that token; a base that already *is*
/// the resolved native gets none (no-op echo); and a hint outside the requested viewport range is
/// filtered out.
#[test]
fn inlay_hint_shows_the_native_ancestor_and_filters_to_the_requested_range() {
    let uri = Uri::from_str("file:///scratch/inlay.otui").expect("uri");
    // Widget < UIWidget: base already is the native class -> no hint.
    // Foo < Widget, Bar < Widget: both resolve to native UIWidget, which differs from the literal
    // "Widget" written -> both get a hint.
    let source = "Widget < UIWidget\nFoo < Widget\nBar < Widget\n";

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");
    let _ = recv_diagnostics(&client, &uri);

    // First: the whole-document viewport must surface both Foo's and Bar's hints (and never one
    // for Widget's own already-native base).
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/inlayHint".to_owned(),
            InlayHintParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: lsp_types::Range::new(Position::new(0, 0), Position::new(3, 0)),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send inlayHint (whole document)");
    let resp = recv_response(&client, &RequestId::from(2));
    assert!(resp.error.is_none(), "inlayHint errored: {resp:?}");
    let hints: Vec<lsp_types::InlayHint> =
        serde_json::from_value(resp.result.expect("inlayHint result present"))
            .expect("deserialize Vec<InlayHint>");

    let foo_pos = base_end_position(source, 1, "Widget");
    let bar_pos = base_end_position(source, 2, "Widget");
    assert_eq!(
        hints.len(),
        2,
        "Widget's own (already-native) base must not get a hint: {hints:#?}"
    );
    assert!(
        hints.iter().any(|h| h.position == foo_pos),
        "Foo's hint missing at {foo_pos:?}: {hints:#?}"
    );
    assert!(
        hints.iter().any(|h| h.position == bar_pos),
        "Bar's hint missing at {bar_pos:?}: {hints:#?}"
    );
    for hint in &hints {
        let label = match &hint.label {
            lsp_types::InlayHintLabel::String(s) => s.clone(),
            lsp_types::InlayHintLabel::LabelParts(_) => panic!("expected a string label"),
        };
        assert!(
            label.contains("UIWidget"),
            "label must name the resolved native ancestor: {label:?}"
        );
    }

    // Second: a viewport scoped to just Foo's line (line 1) must filter Bar's hint out. The end
    // is line 2 column 0 (the start of Bar's line), well clear of `foo_pos` — not clamped to it —
    // so this exercises the "Bar is outside the viewport" filter, not the end-exclusive boundary
    // (that is covered separately below).
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(3),
            "textDocument/inlayHint".to_owned(),
            InlayHintParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: lsp_types::Range::new(Position::new(1, 0), Position::new(2, 0)),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send inlayHint (line 1 only)");
    let resp = recv_response(&client, &RequestId::from(3));
    assert!(resp.error.is_none(), "inlayHint errored: {resp:?}");
    let scoped_hints: Vec<lsp_types::InlayHint> =
        serde_json::from_value(resp.result.expect("inlayHint result present"))
            .expect("deserialize Vec<InlayHint>");

    assert_eq!(
        scoped_hints.len(),
        1,
        "only Foo's hint (line 1) should survive the range filter: {scoped_hints:#?}"
    );
    assert_eq!(scoped_hints[0].position, foo_pos);
    assert!(
        !scoped_hints.iter().any(|h| h.position == bar_pos),
        "Bar's hint (line 2) is outside the requested range and must be filtered out: \
         {scoped_hints:#?}"
    );

    // Third: LSP ranges are end-exclusive, so a viewport whose end sits exactly at Bar's hint
    // anchor must NOT include it — that anchor is one past the requested range, not inside it.
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(4),
            "textDocument/inlayHint".to_owned(),
            InlayHintParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: lsp_types::Range::new(Position::new(2, 0), bar_pos),
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send inlayHint (range ending exactly at Bar's hint)");
    let resp = recv_response(&client, &RequestId::from(4));
    assert!(resp.error.is_none(), "inlayHint errored: {resp:?}");
    let boundary_hints: Vec<lsp_types::InlayHint> =
        serde_json::from_value(resp.result.expect("inlayHint result present"))
            .expect("deserialize Vec<InlayHint>");
    assert!(
        boundary_hints.is_empty(),
        "a hint anchored exactly at the range's (exclusive) end must be excluded: \
         {boundary_hints:#?}"
    );

    shutdown_and_exit(&client, server_thread, 5);
}
