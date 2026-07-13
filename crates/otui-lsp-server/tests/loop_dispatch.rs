//! End-to-end transport test: drive the real handshake + dispatch loop over an in-memory
//! [`lsp_server::Connection`] (no stdio), proving `initialize → didOpen → hover → shutdown/exit`
//! works through `Backend::handle_request`/`handle_notification`.

use std::path::Path;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    DiagnosticSeverity, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, FileChangeType, FileEvent, HoverParams,
    InitializeParams, InitializedParams, Location, NumberOrString, PartialResultParams, Position,
    PublishDiagnosticsParams, ReferenceContext, ReferenceParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    VersionedTextDocumentIdentifier, WorkDoneProgressParams, WorkspaceFolder,
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

/// Send a `textDocument/references` request for `uri`/`position` and decode the response into
/// `Option<Vec<Location>>` (the LSP-null vs. empty-array distinction collapses to `None` vs.
/// `Some(vec![])`, matching what `serde_json` gives back for a JSON `null` result either way).
fn send_references(
    client: &Connection,
    id: i32,
    uri: &Uri,
    position: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(id),
            "textDocument/references".to_owned(),
            ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration,
                },
            },
        )))
        .expect("send references");
    let resp = recv_response(client, &RequestId::from(id));
    assert!(resp.error.is_none(), "references errored: {resp:?}");
    resp.result
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v).expect("Vec<Location>"))
}

/// Read from the client end until the `n`th `textDocument/publishDiagnostics` notification for
/// `uri` has arrived (1-based), skipping everything else in between. Used to detect "the background
/// workspace scan's completion refresh has run": `did_open` always sends the FIRST diagnostics push
/// synchronously, so waiting for a SECOND one for the same `uri` is a deterministic (non-sleeping)
/// signal that the scan — which republishes diagnostics for every open document once it finishes —
/// has completed, and so has whatever it indexes (here, `lua_ref_index`).
///
/// Bounded by [`RECV_TIMEOUT`] for the same reason as [`recv_diagnostics`].
fn wait_for_nth_diagnostics(client: &Connection, uri: &Uri, n: usize) -> PublishDiagnosticsParams {
    let mut count = 0usize;
    loop {
        match client.receiver.recv_timeout(RECV_TIMEOUT) {
            Ok(Message::Notification(note)) if note.method == "textDocument/publishDiagnostics" => {
                let params: PublishDiagnosticsParams = serde_json::from_value(note.params)
                    .expect("deserialize PublishDiagnosticsParams");
                if &params.uri == uri {
                    count += 1;
                    if count == n {
                        return params;
                    }
                }
            }
            Ok(_) => continue,
            Err(e) => panic!(
                "timed out after {RECV_TIMEOUT:?} waiting for publishDiagnostics #{n} for {uri:?} \
                 (server channel: {e})"
            ),
        }
    }
}

/// The `[start, start + needle.len())` LSP [`lsp_types::Range`] of the first occurrence of `needle`
/// in `text` — pairs with [`position_of`] (same ASCII-only assumption) to assert a `Location`'s exact
/// range, not just its containing document.
fn range_of(text: &str, needle: &str) -> lsp_types::Range {
    let start = position_of(text, needle);
    let end = Position::new(start.line, start.character + needle.len() as u32);
    lsp_types::Range { start, end }
}

/// The OTUI↔Lua id cross-reference bridge (spec §2.3), driven entirely through files on disk and the
/// background workspace scan — no `.lua` document is ever opened in this test. This exercises:
///
/// * **Forward** (OTUI `id:` → Lua): `textDocument/references` on `login.otui`'s `id: closeButton`
///   must include, beyond the usual document-local result, the `getChildById('closeButton')` call in
///   the PAIRED `login.lua` — found purely from the disk scan's `lua_ref_index` entry, proving the
///   startup-scan fix (a `.lua` file with refs but no widget defs must not be skipped by the
///   `defs.is_empty()` continue).
/// * **Scoping (negative)**: an unrelated `other/other.lua` references the SAME id string
///   (`closeButton`) but is not `login.otui`'s pair — its location must never appear. This is the
///   correctness boundary the whole node rests on (workspace-wide `LuaRefIndex::lookup` would leak
///   it; `LuaRefIndex::document` on the paired doc only must not).
/// * **Reverse** (Lua → OTUI): `textDocument/references` on the `closeButton` argument inside
///   `login.lua`'s `getChildById` call — again never opened — must resolve back to the `id:`
///   declaration in `login.otui`, and must resolve to NOTHING for `other.lua` (it has no paired
///   `.otui` on disk at all).
#[test]
fn otui_lua_bridge_resolves_both_directions_via_the_disk_scan() {
    let base = std::env::temp_dir().join(format!(
        "otui-lua-bridge-disk-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());

    let login_dir = base.join("modules").join("login");
    std::fs::create_dir_all(&login_dir).expect("mkdir login");
    let login_otui_src = "MainWindow < UIWidget\n  Button\n    id: closeButton\n";
    let login_otui_path = login_dir.join("login.otui");
    std::fs::write(&login_otui_path, login_otui_src).expect("write login.otui");

    let login_lua_src =
        "function onCreate(rootWidget)\n  local btn = rootWidget:getChildById('closeButton')\n  \
         btn:hide()\nend\n";
    let login_lua_path = login_dir.join("login.lua");
    std::fs::write(&login_lua_path, login_lua_src).expect("write login.lua");

    // A DIFFERENT module, unpaired with login.otui (different directory AND stem), that happens to
    // reference the very same id string. Its location must never leak into either direction.
    let other_dir = base.join("modules").join("other");
    std::fs::create_dir_all(&other_dir).expect("mkdir other");
    let other_lua_src =
        "function onCreate(rootWidget)\n  local btn = rootWidget:getChildById('closeButton')\nend\n";
    let other_lua_path = other_dir.join("other.lua");
    std::fs::write(&other_lua_path, other_lua_src).expect("write other.lua");

    let login_otui_uri = file_uri(&login_otui_path);
    let login_lua_uri = file_uri(&login_lua_path);
    let other_lua_uri = file_uri(&other_lua_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    #[allow(deprecated)]
    client_handshake_with_params(
        &client,
        InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: file_uri(&base),
                name: "ws".to_owned(),
            }]),
            ..InitializeParams::default()
        },
    );

    // Open only the `.otui` file — deliberately never `login.lua`/`other.lua` — so every Lua-side
    // result in this test can only have come from the disk scan.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: login_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: login_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    // First push is `did_open`'s own synchronous publish; the second is the scan's completion
    // refresh, which only fires once BOTH the `.otui` and `.lua` scans (including `lua_ref_index`)
    // have finished — see `wait_for_nth_diagnostics`'s doc comment. One call, `n = 2`: each call
    // starts counting from scratch over newly received messages, so two separate `n = 1` calls would
    // actually wait for the 3rd push overall, not the 2nd.
    let _ = wait_for_nth_diagnostics(&client, &login_otui_uri, 2);

    // --- Forward: id: closeButton -> its uses, scoped to the paired login.lua only. ---
    let forward = send_references(
        &client,
        2,
        &login_otui_uri,
        position_of(login_otui_src, "closeButton"),
        true,
    )
    .expect("forward references present");

    let in_login_lua: Vec<&Location> = forward.iter().filter(|l| l.uri == login_lua_uri).collect();
    assert_eq!(
        in_login_lua.len(),
        1,
        "the paired login.lua's getChildById call must appear exactly once: {forward:#?}"
    );
    assert_eq!(
        in_login_lua[0].range,
        range_of(login_lua_src, "closeButton"),
        "the location must land on the id token inside the quotes, not the whole call"
    );
    assert!(
        forward.iter().all(|l| l.uri != other_lua_uri),
        "an unpaired module referencing the same id string must never appear: {forward:#?}"
    );
    // The pre-existing OTUI-local declaration is still present alongside the bridged result.
    assert!(
        forward
            .iter()
            .any(|l| l.uri == login_otui_uri && l.range == range_of(login_otui_src, "closeButton")),
        "the local id: declaration must still be included: {forward:#?}"
    );

    // --- Reverse: the getChildById argument in login.lua -> the id: declaration in login.otui. ---
    let reverse = send_references(
        &client,
        3,
        &login_lua_uri,
        position_of(login_lua_src, "closeButton"),
        true,
    )
    .expect("reverse references present");
    assert_eq!(
        reverse.len(),
        1,
        "exactly one declaration site: {reverse:#?}"
    );
    assert_eq!(reverse[0].uri, login_otui_uri);
    assert_eq!(reverse[0].range, range_of(login_otui_src, "closeButton"));

    // --- Reverse, unpaired: other.lua has no `.otui` sibling on disk at all -> nothing resolves. ---
    let reverse_unpaired = send_references(
        &client,
        4,
        &other_lua_uri,
        position_of(other_lua_src, "closeButton"),
        true,
    )
    .expect("reverse (unpaired) still answers Some, just empty");
    assert!(
        reverse_unpaired.is_empty(),
        "other.lua has no paired .otui, so nothing should resolve: {reverse_unpaired:#?}"
    );

    shutdown_and_exit(&client, server_thread, 5);
}

/// The forward direction of the bridge must see an **unsaved** edit to an open `.lua` buffer — not
/// just what is on disk — so a controller mid-edit still resolves. Exercises
/// `Backend::reindex_lua_refs_open` (wired from `did_open`/`did_change`) with no workspace root at
/// all, so only the open-buffer path is in play (there is no background scan to race against).
#[test]
fn forward_references_see_an_unsaved_lua_buffer_edit() {
    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: closeButton\n";
    let panel_otui_uri = Uri::from_str("file:///scratch/panel.otui").expect("uri");
    let panel_lua_uri = Uri::from_str("file:///scratch/panel.lua").expect("uri");

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: panel_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: panel_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen otui");
    let _ = recv_diagnostics(&client, &panel_otui_uri);

    // Open the paired .lua buffer with text that does NOT yet reference the id.
    let initial_lua = "-- nothing here yet\n";
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: panel_lua_uri.clone(),
                    language_id: "lua".to_owned(),
                    version: 1,
                    text: initial_lua.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    // A `.lua` document still gets an (empty) diagnostics push (the language guard) — wait for it so
    // the didOpen has been fully processed before the references request below.
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    // Sanity: before the edit, the bridge finds nothing in panel.lua.
    let before = send_references(
        &client,
        2,
        &panel_otui_uri,
        position_of(panel_otui_src, "closeButton"),
        false,
    )
    .expect("references present");
    assert!(
        before.iter().all(|l| l.uri != panel_lua_uri),
        "no reference exists yet: {before:#?}"
    );

    // Edit the (still unsaved) buffer to add a getChildById call.
    let edited_lua = "-- nothing here yet\nlocal btn = rootWidget:getChildById('closeButton')\n";
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didChange".to_owned(),
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: panel_lua_uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: edited_lua.to_owned(),
                }],
            },
        )))
        .expect("send didChange lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    let after = send_references(
        &client,
        3,
        &panel_otui_uri,
        position_of(panel_otui_src, "closeButton"),
        false,
    )
    .expect("references present");
    let in_lua: Vec<&Location> = after.iter().filter(|l| l.uri == panel_lua_uri).collect();
    assert_eq!(
        in_lua.len(),
        1,
        "the unsaved edit must be reflected immediately, without a save: {after:#?}"
    );
    assert_eq!(in_lua[0].range, range_of(edited_lua, "closeButton"));

    shutdown_and_exit(&client, server_thread, 4);
}

/// The reverse bridge must resolve an id that is **not** declared anywhere in the paired `.otui`
/// itself, but only in the body of a style it instantiates (spec §2.3, `IdOrigin::InheritedStyle` —
/// see `otui_core::ids`'s module docs: "a quarter of all Lua→OTUI id references resolve into an
/// inherited style rather than the paired file").
///
/// Three files, all found purely through the background workspace scan (nothing but the module
/// `.otui` is ever opened, mirroring `otui_lua_bridge_resolves_both_directions_via_the_disk_scan`
/// above):
///
/// * `styles/base.otui` declares style `MiniWindow`, whose body declares `id: closeButton`.
/// * `mod/mod.otui` instantiates it (`X < MiniWindow`) and declares no id of its own.
/// * `mod/mod.lua` — `mod.otui`'s pair — calls `getChildById('closeButton')`.
///
/// `textDocument/references` on that call must resolve to the `id:` declaration inside
/// `styles/base.otui` — the file that actually declares it — not `mod.otui` (which has no such
/// declaration to point at).
#[test]
fn reverse_references_resolve_an_id_inherited_from_a_base_style() {
    let base = std::env::temp_dir().join(format!(
        "otui-lua-bridge-inherited-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());

    let styles_dir = base.join("styles");
    std::fs::create_dir_all(&styles_dir).expect("mkdir styles");
    let base_otui_src = "MiniWindow < UIWidget\n  Button\n    id: closeButton\n";
    let base_otui_path = styles_dir.join("base.otui");
    std::fs::write(&base_otui_path, base_otui_src).expect("write base.otui");

    let mod_dir = base.join("mod");
    std::fs::create_dir_all(&mod_dir).expect("mkdir mod");
    let mod_otui_src = "X < MiniWindow\n";
    let mod_otui_path = mod_dir.join("mod.otui");
    std::fs::write(&mod_otui_path, mod_otui_src).expect("write mod.otui");

    let mod_lua_src =
        "function onCreate(rootWidget)\n  local btn = rootWidget:getChildById('closeButton')\nend\n";
    let mod_lua_path = mod_dir.join("mod.lua");
    std::fs::write(&mod_lua_path, mod_lua_src).expect("write mod.lua");

    let base_otui_uri = file_uri(&base_otui_path);
    let mod_otui_uri = file_uri(&mod_otui_path);
    let mod_lua_uri = file_uri(&mod_lua_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    #[allow(deprecated)]
    client_handshake_with_params(
        &client,
        InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: file_uri(&base),
                name: "ws".to_owned(),
            }]),
            ..InitializeParams::default()
        },
    );

    // Open only mod.otui — never base.otui, never mod.lua — so both the ancestry resolution
    // (base.otui's style def) and the getChildById call (mod.lua) can only have come from the scan.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: mod_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: mod_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    // Wait for the scan's completion refresh (see `wait_for_nth_diagnostics`'s doc comment): only
    // then are `style_index` (base.otui's `MiniWindow` def) and `lua_ref_index`/`lua_texts` (mod.lua's
    // getChildById call) guaranteed populated.
    let _ = wait_for_nth_diagnostics(&client, &mod_otui_uri, 2);

    let reverse = send_references(
        &client,
        2,
        &mod_lua_uri,
        position_of(mod_lua_src, "closeButton"),
        true,
    )
    .expect("reverse references present");

    assert_eq!(
        reverse.len(),
        1,
        "exactly one declaration site, in the base style file: {reverse:#?}"
    );
    assert_eq!(
        reverse[0].uri, base_otui_uri,
        "the id is declared in the INHERITED style's body, not the instantiating module: {reverse:#?}"
    );
    assert_ne!(
        reverse[0].uri, mod_otui_uri,
        "mod.otui declares no id of its own; it must never be the resolved location"
    );
    assert_eq!(
        reverse[0].range,
        range_of(base_otui_src, "closeButton"),
        "the range must land on the id: value inside base.otui: {reverse:#?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// `did_close` on a `.lua` buffer must re-sync `lua_ref_index` from **disk**, discarding whatever the
/// (possibly unsaved) buffer held — never leaving the closed-over edit's entries in place, and never
/// dropping the file outright (it still exists on disk).
///
/// Sequence: disk holds `getChildById('idAaa')`; open the buffer and edit it in place to
/// `getChildById('idBbb')` (forward references for `idBbb` must reflect the live edit); close the
/// buffer; forward references must then reflect `idAaa` again (disk) and no longer find `idBbb` (the
/// edit is gone, and was never saved).
#[test]
fn did_close_reverts_lua_ref_index_to_disk_content() {
    let base = std::env::temp_dir().join(format!(
        "otui-lua-bridge-close-revert-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&base).expect("mkdir base");
    let _cleanup = TempDirGuard(base.clone());

    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: idAaa\n  Button\n    id: idBbb\n";
    let panel_otui_path = base.join("panel.otui");
    std::fs::write(&panel_otui_path, panel_otui_src).expect("write panel.otui");

    let disk_lua_src = "function onCreate(rootWidget)\n  rootWidget:getChildById('idAaa')\nend\n";
    let panel_lua_path = base.join("panel.lua");
    std::fs::write(&panel_lua_path, disk_lua_src).expect("write panel.lua");

    let panel_otui_uri = file_uri(&panel_otui_path);
    let panel_lua_uri = file_uri(&panel_lua_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    // Open panel.otui.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: panel_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: panel_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen otui");
    let _ = recv_diagnostics(&client, &panel_otui_uri);

    // Open panel.lua with exactly the on-disk content.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: panel_lua_uri.clone(),
                    language_id: "lua".to_owned(),
                    version: 1,
                    text: disk_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    // Baseline: the open buffer (== disk content) resolves 'a', not 'b'.
    let baseline_a = send_references(
        &client,
        2,
        &panel_otui_uri,
        position_of(panel_otui_src, "idAaa"),
        false,
    )
    .expect("references present");
    assert!(
        baseline_a.iter().any(|l| l.uri == panel_lua_uri),
        "the disk-matching open buffer must resolve 'a': {baseline_a:#?}"
    );

    // Edit the (unsaved) buffer: 'a' -> 'b'. Disk is untouched.
    let edited_lua = "function onCreate(rootWidget)\n  rootWidget:getChildById('idBbb')\nend\n";
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didChange".to_owned(),
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: panel_lua_uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: edited_lua.to_owned(),
                }],
            },
        )))
        .expect("send didChange lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    // While the edit is live (still unsaved), forward references must reflect 'b', not 'a'.
    let after_edit_b = send_references(
        &client,
        3,
        &panel_otui_uri,
        position_of(panel_otui_src, "idBbb"),
        false,
    )
    .expect("references present");
    assert!(
        after_edit_b.iter().any(|l| l.uri == panel_lua_uri),
        "the live unsaved edit must be reflected immediately: {after_edit_b:#?}"
    );
    let after_edit_a = send_references(
        &client,
        4,
        &panel_otui_uri,
        position_of(panel_otui_src, "idAaa"),
        false,
    )
    .expect("references present");
    assert!(
        after_edit_a.iter().all(|l| l.uri != panel_lua_uri),
        "'a' no longer appears in the edited buffer: {after_edit_a:#?}"
    );

    // Close the buffer WITHOUT saving. did_close must re-sync from disk: 'a' comes back, 'b' — which
    // was never written to disk — must disappear again.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didClose".to_owned(),
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: panel_lua_uri.clone(),
                },
            },
        )))
        .expect("send didClose lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    let after_close_a = send_references(
        &client,
        5,
        &panel_otui_uri,
        position_of(panel_otui_src, "idAaa"),
        false,
    )
    .expect("references present");
    assert!(
        after_close_a.iter().any(|l| l.uri == panel_lua_uri),
        "closing must re-sync from disk, reviving 'a' — the file was never dropped from the index: \
         {after_close_a:#?}"
    );
    let after_close_b = send_references(
        &client,
        6,
        &panel_otui_uri,
        position_of(panel_otui_src, "idBbb"),
        false,
    )
    .expect("references present");
    assert!(
        after_close_b.iter().all(|l| l.uri != panel_lua_uri),
        "'b' was only ever an unsaved edit; closing must discard it, not persist it: \
         {after_close_b:#?}"
    );

    shutdown_and_exit(&client, server_thread, 7);
}

/// A watched-file `DELETE` for a `.lua` module must drop its entries from `lua_ref_index` entirely —
/// not leave a stale, now-unresolvable entry behind. Exercises `apply_lua_watch_change`'s `DELETED`
/// arm ([`Backend::deindex_lua_refs`]) via `workspace/didChangeWatchedFiles`, independent of the
/// initial scan or any open buffer.
#[test]
fn watched_delete_drops_the_lua_ref_index_entry() {
    let base = std::env::temp_dir().join(format!(
        "otui-lua-bridge-watch-delete-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&base).expect("mkdir base");
    let _cleanup = TempDirGuard(base.clone());

    let foo_otui_src = "Foo < UIWidget\n  Button\n    id: target\n";
    let foo_otui_path = base.join("foo.otui");
    std::fs::write(&foo_otui_path, foo_otui_src).expect("write foo.otui");

    let foo_lua_src = "function onCreate(rootWidget)\n  rootWidget:getChildById('target')\nend\n";
    let foo_lua_path = base.join("foo.lua");
    std::fs::write(&foo_lua_path, foo_lua_src).expect("write foo.lua");

    let foo_otui_uri = file_uri(&foo_otui_path);
    let foo_lua_uri = file_uri(&foo_lua_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: foo_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: foo_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen otui");
    let _ = recv_diagnostics(&client, &foo_otui_uri);

    // Index foo.lua purely via a watched-file CREATED event — no scan, no open buffer.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "workspace/didChangeWatchedFiles".to_owned(),
            DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: foo_lua_uri.clone(),
                    typ: FileChangeType::CREATED,
                }],
            },
        )))
        .expect("send didChangeWatchedFiles created");

    let before = send_references(
        &client,
        2,
        &foo_otui_uri,
        position_of(foo_otui_src, "target"),
        false,
    )
    .expect("references present");
    assert!(
        before.iter().any(|l| l.uri == foo_lua_uri),
        "the watched CREATED event must index foo.lua's reference: {before:#?}"
    );

    // Now fire a DELETE for the same file.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "workspace/didChangeWatchedFiles".to_owned(),
            DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: foo_lua_uri.clone(),
                    typ: FileChangeType::DELETED,
                }],
            },
        )))
        .expect("send didChangeWatchedFiles deleted");

    let after = send_references(
        &client,
        3,
        &foo_otui_uri,
        position_of(foo_otui_src, "target"),
        false,
    )
    .expect("references present");
    assert!(
        after.iter().all(|l| l.uri != foo_lua_uri),
        "the DELETE must drop foo.lua's entry from lua_ref_index, not leave it stale: {after:#?}"
    );

    shutdown_and_exit(&client, server_thread, 4);
}

/// A watched-file `CHANGED` event for a `.lua` module that is **currently open** must not clobber
/// `lua_ref_index`/`lua_texts` with stale disk text: the open buffer is the source of truth for the
/// ref index (kept current by `did_change` → `reindex_lua_refs_open`), so `apply_lua_watch_change`
/// must skip the disk reindex for it — mirroring the `is_open` guard the `.otui` branch of
/// `did_change_watched_files` already applies before its own `index_from_disk`.
///
/// Sequence: disk holds `getChildById('idDisk')`; open the buffer and edit it (unsaved) to
/// `getChildById('idBuf')`; fire a watched `CHANGED` event for that same uri. Forward references
/// must still reflect the buffer (`idBuf`), never fall back to the stale disk scan (`idDisk`).
#[test]
fn watched_change_does_not_clobber_an_open_lua_buffer() {
    let base = std::env::temp_dir().join(format!(
        "otui-lua-bridge-watch-change-open-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&base).expect("mkdir base");
    let _cleanup = TempDirGuard(base.clone());

    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: idDisk\n  Button\n    id: idBuf\n";
    let panel_otui_path = base.join("panel.otui");
    std::fs::write(&panel_otui_path, panel_otui_src).expect("write panel.otui");

    let disk_lua_src = "function onCreate(rootWidget)\n  rootWidget:getChildById('idDisk')\nend\n";
    let panel_lua_path = base.join("panel.lua");
    std::fs::write(&panel_lua_path, disk_lua_src).expect("write panel.lua");

    let panel_otui_uri = file_uri(&panel_otui_path);
    let panel_lua_uri = file_uri(&panel_lua_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: panel_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: panel_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen otui");
    let _ = recv_diagnostics(&client, &panel_otui_uri);

    // Open panel.lua with the on-disk content, then edit it (unsaved) to reference 'idBuf' instead.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: panel_lua_uri.clone(),
                    language_id: "lua".to_owned(),
                    version: 1,
                    text: disk_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    let edited_lua = "function onCreate(rootWidget)\n  rootWidget:getChildById('idBuf')\nend\n";
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didChange".to_owned(),
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: panel_lua_uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: edited_lua.to_owned(),
                }],
            },
        )))
        .expect("send didChange lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    // Baseline before the watch event: the live edit already resolves 'idBuf'.
    let baseline_buf = send_references(
        &client,
        2,
        &panel_otui_uri,
        position_of(panel_otui_src, "idBuf"),
        false,
    )
    .expect("references present");
    assert!(
        baseline_buf.iter().any(|l| l.uri == panel_lua_uri),
        "the live unsaved edit must resolve 'idBuf' before any watch event: {baseline_buf:#?}"
    );

    // A watched CHANGED event fires for the same, still-open uri (disk still says 'idDisk' — the
    // watcher does not know or care that the change came from elsewhere).
    client
        .sender
        .send(Message::Notification(Notification::new(
            "workspace/didChangeWatchedFiles".to_owned(),
            DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: panel_lua_uri.clone(),
                    typ: FileChangeType::CHANGED,
                }],
            },
        )))
        .expect("send didChangeWatchedFiles changed");

    // The watch event must NOT clobber the open buffer's ref index: 'idBuf' must still resolve, and
    // 'idDisk' — only present on disk, never in the live buffer — must still NOT resolve.
    let after_watch_buf = send_references(
        &client,
        3,
        &panel_otui_uri,
        position_of(panel_otui_src, "idBuf"),
        false,
    )
    .expect("references present");
    assert!(
        after_watch_buf.iter().any(|l| l.uri == panel_lua_uri),
        "a watched CHANGED event for an open buffer must not clobber its ref index — 'idBuf' must \
         still resolve: {after_watch_buf:#?}"
    );
    let after_watch_disk = send_references(
        &client,
        4,
        &panel_otui_uri,
        position_of(panel_otui_src, "idDisk"),
        false,
    )
    .expect("references present");
    assert!(
        after_watch_disk.iter().all(|l| l.uri != panel_lua_uri),
        "the watch event must not fall back to the stale disk scan — 'idDisk' must not resolve \
         while the buffer (which never mentions it) is open: {after_watch_disk:#?}"
    );

    shutdown_and_exit(&client, server_thread, 5);
}

/// A watched-file `DELETE` event for a `.lua` module that is **currently open** must not deindex
/// `lua_ref_index`/`lua_texts` either: the buffer is still the source of truth (it may not even
/// correspond to what got deleted on disk — e.g. a save-as-rename momentarily deletes the old path),
/// and `did_close` is what eventually re-syncs (or drops) the entry once the buffer actually closes.
///
/// Sequence: open a `.lua` buffer and edit it (unsaved) to `getChildById('idBuf')`; fire a watched
/// `DELETE` event for that same uri. Forward references must still resolve `idBuf` from the buffer.
#[test]
fn watched_delete_does_not_clobber_an_open_lua_buffer() {
    let base = std::env::temp_dir().join(format!(
        "otui-lua-bridge-watch-delete-open-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&base).expect("mkdir base");
    let _cleanup = TempDirGuard(base.clone());

    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: idBuf\n";
    let panel_otui_path = base.join("panel.otui");
    std::fs::write(&panel_otui_path, panel_otui_src).expect("write panel.otui");

    let disk_lua_src = "function onCreate(rootWidget)\nend\n";
    let panel_lua_path = base.join("panel.lua");
    std::fs::write(&panel_lua_path, disk_lua_src).expect("write panel.lua");

    let panel_otui_uri = file_uri(&panel_otui_path);
    let panel_lua_uri = file_uri(&panel_lua_path);

    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: panel_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: panel_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen otui");
    let _ = recv_diagnostics(&client, &panel_otui_uri);

    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: panel_lua_uri.clone(),
                    language_id: "lua".to_owned(),
                    version: 1,
                    text: disk_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    let edited_lua = "function onCreate(rootWidget)\n  rootWidget:getChildById('idBuf')\nend\n";
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didChange".to_owned(),
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: panel_lua_uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: edited_lua.to_owned(),
                }],
            },
        )))
        .expect("send didChange lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    // A watched DELETE event fires for the same, still-open uri.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "workspace/didChangeWatchedFiles".to_owned(),
            DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: panel_lua_uri.clone(),
                    typ: FileChangeType::DELETED,
                }],
            },
        )))
        .expect("send didChangeWatchedFiles deleted");

    let after_delete = send_references(
        &client,
        2,
        &panel_otui_uri,
        position_of(panel_otui_src, "idBuf"),
        false,
    )
    .expect("references present");
    assert!(
        after_delete.iter().any(|l| l.uri == panel_lua_uri),
        "a watched DELETE event for an open buffer must not deindex it — 'idBuf' must still \
         resolve from the live buffer: {after_delete:#?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}
