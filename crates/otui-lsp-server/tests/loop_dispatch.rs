//! End-to-end transport test: drive the real handshake + dispatch loop over an in-memory
//! [`lsp_server::Connection`] (no stdio), proving `initialize → didOpen → hover → shutdown/exit`
//! works through `Backend::handle_request`/`handle_notification`.

use std::path::Path;
use std::str::FromStr;
use std::thread;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    DiagnosticSeverity, DidOpenTextDocumentParams, HoverParams, InitializeParams,
    InitializedParams, NumberOrString, Position, PublishDiagnosticsParams, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Uri, WorkDoneProgressParams, WorkspaceFolder,
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

/// Read from the client end until the [`Response`] for `id` arrives, skipping anything else the
/// server pushed in the meantime (log notifications, `client/registerCapability` requests, …).
fn recv_response(client: &Connection, id: &RequestId) -> Response {
    loop {
        match client.receiver.recv().expect("server channel open") {
            Message::Response(resp) if &resp.id == id => return resp,
            _ => continue,
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
fn recv_diagnostics(client: &Connection, uri: &Uri) -> PublishDiagnosticsParams {
    loop {
        match client.receiver.recv().expect("server channel open") {
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(n.params).expect("deserialize PublishDiagnosticsParams");
                if &params.uri == uri {
                    return params;
                }
            }
            _ => continue,
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
