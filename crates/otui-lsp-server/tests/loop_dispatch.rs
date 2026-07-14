//! End-to-end transport test: drive the real handshake + dispatch loop over an in-memory
//! [`lsp_server::Connection`] (no stdio), proving `initialize → didOpen → hover → shutdown/exit`
//! works through `Backend::handle_request`/`handle_notification`.

// See `otui_lsp_server::lib`'s own crate-level allow for the rationale: `lsp_types::Uri`'s
// `Hash`/`Eq` are defined purely over `as_str()`, so using it as a map key (a `CodeAction`'s
// `WorkspaceEdit::changes`) is sound despite the interior-mutability false positive.
#![allow(clippy::mutable_key_type)]

use std::path::Path;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, CodeActionContext, CodeActionOrCommand, CodeActionParams,
    CompletionClientCapabilities, CompletionItemCapability, CompletionParams, CompletionResponse,
    DiagnosticSeverity, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, Documentation, FileChangeType,
    FileEvent, GotoDefinitionParams, GotoDefinitionResponse, HoverParams, InitializeParams,
    InitializedParams, InlayHintParams, Location, MarkupKind, NumberOrString, PartialResultParams,
    Position, PublishDiagnosticsParams, ReferenceContext, ReferenceParams,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Uri, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceFolder,
};
use otui_lsp_server::{Backend, Termination, serve};

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

/// [`position_of`]'s counterpart for the LAST occurrence of `needle` in `text` — needed when the
/// same id string legitimately appears twice (e.g. a `setId('x')` definition followed by a
/// `getChildById('x')` reference to it) and the cursor must land on the second one specifically.
fn position_of_last(text: &str, needle: &str) -> Position {
    let idx = text.rfind(needle).expect("needle present in text");
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

/// Hovering a style whose base inherits transitively (`Foo < Bar`, `Bar < UIButton`) must show the
/// **full** resolved chain, not just the first hop — proving the hover render walks all the way to
/// the native class via `resolve_ancestry` rather than stopping at `Bar`.
#[test]
fn memory_connection_hover_shows_the_full_multi_hop_inheritance_chain() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    client_handshake(&client);

    let uri = Uri::from_str("file:///scratch/chain.otui").expect("uri");
    let text = "Foo < Bar\nBar < UIButton\n".to_owned();
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: text.clone(),
                },
            },
        )))
        .expect("send didOpen");

    // Hover over `Foo`, the declared name — its own base is `Bar`, which itself resolves onward to
    // the native `UIButton`.
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: position_of(&text, "Foo"),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    // The chain must reach past the immediate hop (`Bar`) to the ultimately-resolved native class.
    assert!(value.contains("Bar"), "chain must mention `Bar`: {value}");
    assert!(
        value.contains("UIButton"),
        "chain must resolve to the native `UIButton`: {value}"
    );
    assert!(
        value.contains("(built-in)"),
        "the native end of the chain must be marked built-in: {value}"
    );
    // The full arrow chain, in order — not just the two names appearing somewhere in the text.
    assert!(
        value.contains("Bar") && value.find("Bar").unwrap() < value.find("UIButton").unwrap(),
        "chain must read Bar before UIButton: {value}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hovering a **per-widget** property key — one the global catalog does not know, but the enclosing
/// widget's resolved ancestry declares (here, `placeholder`, native `UITextEdit`'s per-widget style
/// tag) — must still return a non-empty hover, naming the widget it resolved against.
#[test]
fn memory_connection_hover_on_a_per_widget_property_describes_it() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    client_handshake(&client);

    let uri = Uri::from_str("file:///scratch/textedit.otui").expect("uri");
    let text = "TextEdit < UITextEdit\nSearchBox < TextEdit\n  placeholder: Search...\n".to_owned();
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: text.clone(),
                },
            },
        )))
        .expect("send didOpen");

    // One character INTO the `placeholder` token, not its exact start: `descendant_for_byte_range`
    // resolves a zero-width range sitting exactly at a token boundary to an ancestor, not the leaf
    // itself (the same reason the unit-level helpers offset `+ 1` past the needle's start).
    let mut position = position_of(&text, "placeholder");
    position.character += 1;
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    assert!(value.contains("`placeholder`"), "{value}");
    // The enclosing widget is `SearchBox`'s style_header, whose `base` field is `TextEdit` — the
    // ancestry-resolution entry point mirrors `completion::enclosing_widget_type`'s own choice (a
    // `style_header`'s enclosing type is its declared base, not its own name).
    assert!(
        value.contains("property of") && value.contains("TextEdit"),
        "expected a widget-aware property hover naming the enclosing widget: {value}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hovering a key nested under a `layout:` block (`num-columns`) — not a global catalog property,
/// but a `layout:`-block key the shared `classify_layout_value` classifier describes — must return a
/// non-empty hover naming the value kind (here, "Takes an integer."), end to end through the real
/// LSP loop with no server-side rendering change (`render_property_hover` already calls the shared
/// `documentation_body`, which now covers layout keys too).
#[test]
fn memory_connection_hover_on_a_layout_block_key_describes_its_value_kind() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    client_handshake(&client);

    let uri = Uri::from_str("file:///scratch/layout.otui").expect("uri");
    let text = "Panel\n  layout:\n    type: grid\n    num-columns: 3\n".to_owned();
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: text.clone(),
                },
            },
        )))
        .expect("send didOpen");

    let mut position = position_of(&text, "num-columns");
    position.character += 1;
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    assert!(value.contains("`num-columns`"), "{value}");
    assert!(value.contains("Takes an integer."), "{value}");
    assert!(value.contains("silently ignored"), "{value}");

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hovering an `&tag:` alias-property key whose value is an ordinary Lua expression (`bar()`, not
/// the `#` carve-out) must show BOTH engine-simultaneous meanings (spec §2.6 / §5.5): the OTML
/// variable/alias AND the Lua-evaluated widget-instance field, with the "evaluated as a Lua
/// expression" wording (not the plain-string carve-out wording).
#[test]
fn memory_connection_hover_on_an_alias_key_shows_both_meanings_eval_variant() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    client_handshake(&client);

    let uri = Uri::from_str("file:///scratch/alias_eval.otui").expect("uri");
    let text = "Panel\n  &foo: bar()\n".to_owned();
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: text.clone(),
                },
            },
        )))
        .expect("send didOpen");

    let mut position = position_of(&text, "foo");
    position.character += 1;
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    assert!(value.contains("`&foo`"), "{value}");
    assert!(value.contains("OTML variable"), "{value}");
    assert!(value.contains("$foo"), "{value}");
    assert!(value.contains("Lua widget field"), "{value}");
    assert!(value.contains("evaluated as a Lua expression"), "{value}");
    assert!(!value.contains("plain string, not"), "{value}");

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hovering an `&tag:` alias-property key whose value hits the §2.6 `#` carve-out (`#33AAFF`) must
/// show both meanings too, but with the "plain string, not evaluated" wording for the Lua-field
/// role — the hex-literal carve-out that lets a color survive instead of parsing as a Lua comment.
#[test]
fn memory_connection_hover_on_an_alias_key_shows_both_meanings_hash_literal_variant() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    client_handshake(&client);

    let uri = Uri::from_str("file:///scratch/alias_hash.otui").expect("uri");
    let text = "Panel\n  &color: #33AAFF\n".to_owned();
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: text.clone(),
                },
            },
        )))
        .expect("send didOpen");

    let mut position = position_of(&text, "color");
    position.character += 1;
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(2));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    assert!(value.contains("`&color`"), "{value}");
    assert!(value.contains("OTML variable"), "{value}");
    assert!(value.contains("$color"), "{value}");
    assert!(value.contains("Lua widget field"), "{value}");
    assert!(value.contains("plain string, not"), "{value}");
    assert!(value.contains("carve-out"), "{value}");
    assert!(!value.contains("evaluated as a Lua expression"), "{value}");

    shutdown_and_exit(&client, server_thread, 3);
}

/// `textDocument/completion` end-to-end, with the client advertising Markdown
/// `documentationFormat`: a completion item for a curated global property (`width`) must come back
/// with its `documentation` populated as Markdown — the curated one-line note surfaced from
/// `property_hover::property_doc`, not just a one-word `detail`.
#[test]
fn memory_connection_drives_initialize_open_completion_shutdown() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));

    // Advertise Markdown documentation support so the server's Markdown branch is exercised.
    let init_params = InitializeParams {
        capabilities: ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                completion: Some(CompletionClientCapabilities {
                    completion_item: Some(CompletionItemCapability {
                        documentation_format: Some(vec![MarkupKind::Markdown]),
                        ..CompletionItemCapability::default()
                    }),
                    ..CompletionClientCapabilities::default()
                }),
                ..TextDocumentClientCapabilities::default()
            }),
            ..ClientCapabilities::default()
        },
        ..InitializeParams::default()
    };
    client_handshake_with_params(&client, init_params);

    // didOpen a document with a half-typed property key on an indented line.
    let uri = Uri::from_str("file:///scratch/completion.otui").expect("uri");
    let text = "Panel < UIWidget\n  wid\n";
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: text.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    // Cursor right after "wid" on line 1.
    let position = position_of(text, "wid");
    let position = Position::new(position.line, position.character + "wid".len() as u32);
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/completion".to_owned(),
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
        )))
        .expect("send completion");
    let completion_resp = recv_response(&client, &RequestId::from(2));
    assert!(
        completion_resp.error.is_none(),
        "completion errored: {completion_resp:?}"
    );
    let completion_value = completion_resp.result.expect("completion result present");
    let response: CompletionResponse =
        serde_json::from_value(completion_value).expect("deserialize CompletionResponse");
    let items = match response {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    let width = items
        .iter()
        .find(|i| i.label == "width")
        .expect("width completion item offered");
    match &width.documentation {
        Some(Documentation::MarkupContent(content)) => {
            assert_eq!(content.kind, MarkupKind::Markdown);
            assert!(
                content.value.contains("dimension"),
                "expected width's curated doc, got {:?}",
                content.value
            );
        }
        other => panic!("expected width to carry Markdown documentation, got {other:?}"),
    }

    shutdown_and_exit(&client, server_thread, 3);
}

/// `textDocument/completion` at a boolean property's **value** position (right after `enabled:`)
/// must return exactly `true`/`false`, not the property-key catalog — the N2 boolean-value-completion
/// slice (`property_hover::PropertyValueKind::Boolean`), exercised end-to-end through the real LSP
/// request/response loop and its byte-offset <-> UTF-16 `Position` conversion, not just the pure
/// `otui-core` unit test.
#[test]
fn completion_at_a_boolean_property_value_position_offers_true_and_false() {
    let (server, client) = Connection::memory();
    let server_thread = thread::spawn(move || run_server(server));
    client_handshake(&client);

    let uri = Uri::from_str("file:///scratch/bool-completion.otui").expect("uri");
    let text = "Panel < UIWidget\n  enabled:\n";
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: text.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let position = position_of_last(text, "enabled:");
    let position = Position::new(position.line, position.character + "enabled:".len() as u32);
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/completion".to_owned(),
            CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                context: None,
            },
        )))
        .expect("send completion");
    let completion_resp = recv_response(&client, &RequestId::from(2));
    assert!(
        completion_resp.error.is_none(),
        "completion errored: {completion_resp:?}"
    );
    let completion_value = completion_resp.result.expect("completion result present");
    let response: CompletionResponse =
        serde_json::from_value(completion_value).expect("deserialize CompletionResponse");
    let items = match response {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    let labels: std::collections::BTreeSet<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(
        labels,
        std::collections::BTreeSet::from(["true", "false"]),
        "expected exactly true/false, got {labels:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
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
    let source = "Panel < UIWidget\n  image-source: |\n    x\n\n    <b>PWN</b> [click](https://evil.example)\n";

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

/// Send a `textDocument/hover` request for `uri`/`position` and return the raw [`Response`] (the
/// caller decodes it — `hover_markdown` for the common Markdown-string case, or a direct `is_null`
/// check for a "no hover" assertion).
fn send_hover(client: &Connection, id: i32, uri: &Uri, position: Position) -> Response {
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(id),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let resp = recv_response(client, &RequestId::from(id));
    assert!(resp.error.is_none(), "hover errored: {resp:?}");
    resp
}

/// Send a `textDocument/definition` request for `uri`/`position` and decode the response into
/// `Option<GotoDefinitionResponse>` (`None` for the LSP-null "no definition" answer).
fn send_goto_definition(
    client: &Connection,
    id: i32,
    uri: &Uri,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(id),
            "textDocument/definition".to_owned(),
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        )))
        .expect("send definition");
    let resp = recv_response(client, &RequestId::from(id));
    assert!(resp.error.is_none(), "definition errored: {resp:?}");
    resp.result
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v).expect("GotoDefinitionResponse"))
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

/// [`send_references`]'s counterpart for `textDocument/definition`.
fn send_definition(
    client: &Connection,
    id: i32,
    uri: &Uri,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(id),
            "textDocument/definition".to_owned(),
            lsp_types::GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        )))
        .expect("send definition");
    let resp = recv_response(client, &RequestId::from(id));
    assert!(resp.error.is_none(), "definition errored: {resp:?}");
    resp.result
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v).expect("GotoDefinitionResponse"))
}

/// Poll `textDocument/references` for `uri`/`position` until `accept` returns `true` for a `Some`
/// result, or the overall [`RECV_TIMEOUT`] deadline elapses — whichever comes first.
///
/// This is the deterministic replacement for a since-retired helper that waited for the SECOND
/// `textDocument/publishDiagnostics` push as a "the background scan has finished" signal: `did_open`
/// sends the first push synchronously, and the scan republishes every open document once it
/// completes, so a second push *looked* like a safe "scan done" marker. It was not: the scan is
/// spawned from `initialized`, so for a workspace this small it can finish (and run its completion
/// refresh) BEFORE the server has even processed the test's `did_open` notification — the refresh
/// then iterates zero open documents, the second push never comes, and the test hangs for the full
/// [`RECV_TIMEOUT`] on a race, not a bug (see the `references` handlers: they read `style_index`/
/// `lua_ref_index`/`lua_texts` live on every call, with no dependency on that diagnostics republish at
/// all — the republish is a UX nicety, not a correctness gate).
///
/// Polling the actual query sidesteps the race entirely: once the scan has indexed what a given
/// query needs, the very next poll observes it, independent of any diagnostics ordering. Each attempt
/// gets a fresh request id, taken from (and incrementing) `*next_id`, so no two in-flight requests in
/// a retry loop ever share an id. Panics with the last-seen result — never a silent pass — if `accept`
/// still rejects everything once the deadline passes.
///
/// Callers should generally pass `accept = |_| true` — "any `Some` means the file this query needs is
/// indexed" — and assert on the *returned* `Vec<Location>` afterward with plain `assert_eq!`/`assert!`,
/// rather than folding the expected content into `accept` itself. That way a genuine product
/// regression is reported as an immediate, specific assertion failure, not as this function's 10s
/// timeout panic (which reads as "the scan never finished" even when it did, and the answer was simply
/// wrong).
fn references_until(
    client: &Connection,
    next_id: &mut i32,
    uri: &Uri,
    position: Position,
    include_declaration: bool,
    mut accept: impl FnMut(&[Location]) -> bool,
) -> Vec<Location> {
    let deadline = Instant::now() + RECV_TIMEOUT;
    let mut last: Option<Vec<Location>> = None;
    loop {
        let id = *next_id;
        *next_id += 1;
        if let Some(locations) = send_references(client, id, uri, position, include_declaration) {
            if accept(&locations) {
                return locations;
            }
            last = Some(locations);
        }
        if Instant::now() >= deadline {
            panic!(
                "textDocument/references for {uri:?} at {position:?} never satisfied the expected \
                 condition within {RECV_TIMEOUT:?} of polling (the background scan likely never \
                 finished indexing what this query needs); last non-null result: {last:?}"
            );
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
///
/// The body below checks these in REVERSE order (reverse, then reverse-unpaired, then forward), not
/// the order listed above: the two reverse queries are polled to convergence via
/// [`references_until`], and convergence on both doubles as proof that the background scan has fully
/// indexed both `login.lua` and `other.lua` — which the forward query's negative-scoping assertion
/// needs to be reliable (see `references_until`'s doc comment for why waiting on a diagnostics count
/// instead was flaky, and the forward query's own comment for why it deliberately runs last,
/// unretried).
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

    let login_lua_src = "function onCreate(rootWidget)\n  local btn = rootWidget:getChildById('closeButton')\n  \
         btn:hide()\nend\n";
    let login_lua_path = login_dir.join("login.lua");
    std::fs::write(&login_lua_path, login_lua_src).expect("write login.lua");

    // A DIFFERENT module, unpaired with login.otui (different directory AND stem), that happens to
    // reference the very same id string. Its location must never leak into either direction.
    let other_dir = base.join("modules").join("other");
    std::fs::create_dir_all(&other_dir).expect("mkdir other");
    let other_lua_src = "function onCreate(rootWidget)\n  local btn = rootWidget:getChildById('closeButton')\nend\n";
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

    // Request ids for everything below are handed out from this counter: `references_until` may
    // issue several attempts per call, so a fixed literal per call (as a single `send_references`
    // would use) is not enough.
    let mut next_id = 2i32;

    // --- Reverse first, polled: the getChildById argument in login.lua -> the id: declaration in
    // login.otui. The poll only waits for readiness (any `Some`) — never for the expected content —
    // so a real bug in the resolution surfaces as a normal, immediate `assert_eq!` failure below, not
    // a 10s helper timeout. `references` reads `lua_ref_index`/`lua_texts` live and a whole file's
    // refs are written in one atomic `set_document` call (never partially), so the first `Some` this
    // sees is already login.lua's fully-indexed, final answer. Converging here also proves login.lua
    // has been fully indexed by the background scan — a prerequisite the forward query's
    // negative-scoping check below relies on.
    let reverse = references_until(
        &client,
        &mut next_id,
        &login_lua_uri,
        position_of(login_lua_src, "closeButton"),
        true,
        |_locs| true,
    );
    assert_eq!(
        reverse.len(),
        1,
        "exactly one declaration site: {reverse:#?}"
    );
    assert_eq!(reverse[0].uri, login_otui_uri);
    assert_eq!(reverse[0].range, range_of(login_otui_src, "closeButton"));

    // --- Reverse, unpaired, polled: other.lua has no `.otui` sibling on disk at all -> nothing
    // resolves. `lua_references` answers `None` (not yet indexed) until the scan reaches other.lua,
    // then `Some([])` forever after (it is permanently unpaired) — so "any `Some` result" is already
    // the converged, correct signal here, and polling for it also proves other.lua has been fully
    // indexed, the second prerequisite for the forward query below.
    let reverse_unpaired = references_until(
        &client,
        &mut next_id,
        &other_lua_uri,
        position_of(other_lua_src, "closeButton"),
        true,
        |_locs| true,
    );
    assert!(
        reverse_unpaired.is_empty(),
        "other.lua has no paired .otui, so nothing should resolve: {reverse_unpaired:#?}"
    );

    // --- Forward: id: closeButton -> its uses, scoped to the paired login.lua only. Both login.lua
    // and other.lua are now confirmed fully indexed (the two polls above), so this single, unretried
    // query already reflects the scan's final state — including for the negative-scoping assertion,
    // which would otherwise risk a false pass if `other.lua` had not been indexed yet at the moment it
    // was checked. ---
    let forward_id = next_id;
    next_id += 1;
    let forward = send_references(
        &client,
        forward_id,
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

    shutdown_and_exit(&client, server_thread, next_id);
}

/// Hover on an `id:` **declaration** value (spec §5.5): "this widget's id" plus a reference count —
/// end to end, through the real disk scan, proving the count the hover renders is EXACTLY the count a
/// `textDocument/references` request (`include_declaration: false`, mirroring the fact that the
/// declaration is never its own reference) on the same token returns. This is not incidental: the
/// hover handler calls the very same `lua_forward_references` the `references` handler calls
/// (`Backend::references`), so the two can never drift apart by construction — this test is the
/// end-to-end proof of that invariant.
///
/// Mirrors `otui_lua_bridge_resolves_both_directions_via_the_disk_scan`'s setup (a paired
/// `.otui`/`.lua` module, only the `.otui` ever opened, so every Lua-side count can only have come
/// from the background disk scan), but adds a same-document anchor-target reference too, so the count
/// mixes BOTH origins — proving the hover's breakdown line as well.
#[test]
fn hover_on_an_id_declaration_reference_count_matches_the_references_response() {
    let base = std::env::temp_dir().join(format!(
        "otui-id-hover-count-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());

    let login_dir = base.join("modules").join("login");
    std::fs::create_dir_all(&login_dir).expect("mkdir login");
    // One anchor-target reference to `closeButton` in this same document, PLUS a `getChildById` use
    // in the paired `.lua` controller below — the count must combine both.
    let login_otui_src = "MainWindow < UIWidget\n  Button\n    id: closeButton\n  \
         Button2\n    anchors.top: closeButton.bottom\n";
    let login_otui_path = login_dir.join("login.otui");
    std::fs::write(&login_otui_path, login_otui_src).expect("write login.otui");

    let login_lua_src = "function onCreate(rootWidget)\n  local btn = rootWidget:getChildById('closeButton')\n  \
         btn:hide()\nend\n";
    let login_lua_path = login_dir.join("login.lua");
    std::fs::write(&login_lua_path, login_lua_src).expect("write login.lua");

    let login_otui_uri = file_uri(&login_otui_path);
    let login_lua_uri = file_uri(&login_lua_path);

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

    // Open only the `.otui` file — never `login.lua` — so the Lua-side half of the count can only
    // have come from the background disk scan, exactly as the sibling bridge test does.
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

    let mut next_id = 2i32;
    let decl_position = position_of(login_otui_src, "closeButton");

    // Poll `references` (declaration excluded, mirroring the hover's own count) until the paired
    // login.lua's getChildById use has been indexed by the background scan — convergence here is what
    // makes the single, unretried hover request below reliable.
    let references = references_until(
        &client,
        &mut next_id,
        &login_otui_uri,
        decl_position,
        false,
        |locs| locs.iter().any(|l| l.uri == login_lua_uri),
    );
    // Exactly two references: the same-document anchor target, plus the paired Lua use.
    assert_eq!(references.len(), 2, "{references:#?}");

    let hover_id = next_id;
    next_id += 1;
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(hover_id),
            "textDocument/hover".to_owned(),
            HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: login_otui_uri.clone(),
                    },
                    position: decl_position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            },
        )))
        .expect("send hover");
    let hover_resp = recv_response(&client, &RequestId::from(hover_id));
    assert!(hover_resp.error.is_none(), "hover errored: {hover_resp:?}");
    let value = hover_markdown(&hover_resp);

    assert!(
        value.contains("**`closeButton`** — this widget's id."),
        "{value}"
    );
    // The load-bearing assertion: the hover's rendered count is EXACTLY `references.len()`, and the
    // breakdown names both origins (1 same-document anchor, 1 paired-Lua use).
    assert!(
        value.contains("2 references (1 anchor in this file, 1 in the paired Lua controller(s))."),
        "{value}"
    );
    assert_eq!(
        references.len(),
        2,
        "hover's rendered total must equal the references response's length"
    );

    shutdown_and_exit(&client, server_thread, next_id);
}

/// The module-association half of the OTUI↔Lua bridge (node `smart-pairing`): a controller and its
/// UI file that share NEITHER a directory NOR a stem — the shape `paired_uri`'s same-directory/
/// same-stem fast path alone cannot resolve — must still pair, via the module's `.otmod` `scripts:`
/// list crossed with the controller's `g_ui.loadUI` call.
///
/// Mirrors `otui_lua_bridge_resolves_both_directions_via_the_disk_scan`'s shape and rationale
/// (reverse-then-reverse-unpaired-then-forward ordering, `references_until` used only as a
/// readiness poll, never to encode the expected content) with the module-association wrinkle:
///
/// * `mymodule/mymodule.otmod` names `ctrl` as its controller (`scripts: [ ctrl ]`).
/// * `mymodule/ctrl.lua` calls `g_ui.loadUI('styles/ui')` and `getChildById('x')`.
/// * `mymodule/styles/ui.otui` declares `id: x` — a DIFFERENT stem AND directory than `ctrl.lua`,
///   so `paired_uri` alone finds nothing here; only the module association does.
/// * `othermodule/othermodule.otmod` + `othermodule/otherctrl.lua` (`getChildById('x')`, but NO
///   `loadUI` call at all) is the **negative** case for the FORWARD direction and for `ctrl.lua`'s
///   OWN module (`mymodule`): same id string, its own `.otmod`, but no association naming `ui.otui`
///   — its location must never appear among `ui.otui`'s forward references, and `decoy.otui` (a
///   DIFFERENT module than `otherctrl.lua`'s own) must never be what `ctrl.lua`'s reverse query
///   resolves to either.
/// * `othermodule/decoy.otui` (`id: x`, never loaded by any controller) is what makes `otherctrl.lua`
///   itself the *positive* case for the module-union fallback (node `module-union-unique`): within
///   `otherctrl.lua`'s OWN module (`othermodule`), `decoy.otui` is the sole `.otui` declaring `x` —
///   exact pairing (no `loadUI` call at all) resolves nothing, so the fallback resolves it there. It
///   must still never appear when resolving `ctrl.lua`'s `getChildById('x')` (a DIFFERENT module,
///   `mymodule`, whose own exact pairing already succeeds — see this node's
///   `lua_bridge_module_union_fallback_covers_unique_ambiguous_and_exact_pairing` for that ranking
///   covered directly).
#[test]
fn module_association_pairs_a_controller_with_a_differently_named_and_located_ui_file() {
    let base = std::env::temp_dir().join(format!(
        "otui-module-assoc-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());

    let my_module_dir = base.join("modules").join("mymodule");
    std::fs::create_dir_all(my_module_dir.join("styles")).expect("mkdir mymodule/styles");
    std::fs::write(
        my_module_dir.join("mymodule.otmod"),
        "Module\n  name: mymodule\n  scripts: [ ctrl ]\n",
    )
    .expect("write mymodule.otmod");
    let ctrl_lua_src = "function onCreate(w)\n  g_ui.loadUI('styles/ui')\n  \
                        local btn = w:getChildById('x')\nend\n";
    let ctrl_lua_path = my_module_dir.join("ctrl.lua");
    std::fs::write(&ctrl_lua_path, ctrl_lua_src).expect("write ctrl.lua");
    let ui_otui_src = "MainWindow < UIWidget\n  Button\n    id: x\n";
    let ui_otui_path = my_module_dir.join("styles").join("ui.otui");
    std::fs::write(&ui_otui_path, ui_otui_src).expect("write ui.otui");

    let other_module_dir = base.join("modules").join("othermodule");
    std::fs::create_dir_all(&other_module_dir).expect("mkdir othermodule");
    std::fs::write(
        other_module_dir.join("othermodule.otmod"),
        "Module\n  name: othermodule\n  scripts: [ otherctrl ]\n",
    )
    .expect("write othermodule.otmod");
    // Same id string, its own real `.otmod`, but NO `loadUI`/`displayUI`/`importStyle` call at all —
    // this controller is associated with nothing.
    let other_ctrl_lua_src = "function onCreate(w)\n  local btn = w:getChildById('x')\nend\n";
    let other_ctrl_lua_path = other_module_dir.join("otherctrl.lua");
    std::fs::write(&other_ctrl_lua_path, other_ctrl_lua_src).expect("write otherctrl.lua");
    // Declares the same id, but no controller ever loads it — must never surface as a reverse
    // navigation target for ctrl.lua's getChildById.
    let decoy_otui_src = "Decoy < UIWidget\n  Button\n    id: x\n";
    let decoy_otui_path = other_module_dir.join("decoy.otui");
    std::fs::write(&decoy_otui_path, decoy_otui_src).expect("write decoy.otui");

    let my_lua_uri = file_uri(&ctrl_lua_path);
    let my_otui_uri = file_uri(&ui_otui_path);
    let other_lua_uri = file_uri(&other_ctrl_lua_path);
    let decoy_otui_uri = file_uri(&decoy_otui_path);

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

    // Open only `ui.otui` — never any `.lua`/`.otmod` file — so every Lua-side result here can only
    // have come from the background disk scan's module-association index.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: my_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: ui_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let mut next_id = 2i32;

    // --- Reverse: ctrl.lua's getChildById('x') -> ui.otui's id: x declaration.
    //
    // Unlike the disk-scan bridge test above, "any `Some`" is NOT a safe readiness signal here:
    // `module_ui_index` is populated by a THIRD phase of the background scan, running strictly
    // AFTER the style/lua-ref phases that make `lua_text_for`/`ref_at` succeed at all — so
    // `lua_references` can legitimately answer `Some([])` (the getChildById token was found, but
    // the module association had not been built yet) well before the real, non-empty answer is
    // ready. Polling until the result is non-empty instead is the correct proxy: `module_ui_index`
    // is swapped in with a single atomic write at the end of that phase
    // (`*module_ui_index.write().expect(..) = built`), so the first non-empty answer already
    // reflects that phase's fully-settled state — exactly like the disk-scan test's "any `Some`"
    // does for its own (single-phase) dependency.
    let reverse = references_until(
        &client,
        &mut next_id,
        &my_lua_uri,
        position_of(ctrl_lua_src, "x"),
        true,
        |locs: &[Location]| !locs.is_empty(),
    );
    assert_eq!(
        reverse.len(),
        1,
        "exactly one declaration site, resolved purely via the module association: {reverse:#?}"
    );
    assert_eq!(reverse[0].uri, my_otui_uri);
    assert_eq!(reverse[0].range, range_of(ui_otui_src, "x"));
    assert!(
        reverse[0].uri != decoy_otui_uri,
        "an otui file no controller ever loads must never be a reverse target"
    );

    // --- Reverse, exact-pairing-less but module-union-resolved: otherctrl.lua calls no
    // loadUI/displayUI/importStyle at all, so it has no exact pairing (no module association, no
    // same-stem sibling either) — the shape this test predates `module-union-unique` documenting as
    // "must never resolve". It now DOES resolve, via the fallback that node adds: `decoy.otui` is the
    // sole `.otui` declaring `x` anywhere in `otherctrl.lua`'s own module (`othermodule`), so a live
    // reference to `x` there can only mean that file (see `resolve_module_unique_id_declaration`'s
    // doc comment for the full rationale). Polled for non-emptiness, the same convergence criterion
    // the reverse poll just above uses, for the same reason (this fallback's own disk-scan
    // dependency, `disk_texts`, is a strictly EARLIER phase than `module_ui_index`, whose completion
    // that first poll already proved, so a plain, unretried call would also be safe here — polling
    // is kept anyway for symmetry/robustness against a future phase-ordering change).
    let reverse_via_fallback = references_until(
        &client,
        &mut next_id,
        &other_lua_uri,
        position_of(other_ctrl_lua_src, "x"),
        true,
        |locs: &[Location]| !locs.is_empty(),
    );
    assert_eq!(
        reverse_via_fallback,
        vec![Location {
            uri: decoy_otui_uri.clone(),
            range: range_of(decoy_otui_src, "x"),
        }],
        "the module-union fallback must resolve otherctrl.lua's getChildById to decoy.otui, the sole \
         declarer of `x` in othermodule: {reverse_via_fallback:#?}"
    );

    // --- Forward: id: x -> its uses, scoped to the associated ctrl.lua only. Both ctrl.lua and
    // otherctrl.lua are now confirmed indexed by the two polls above, so this single, unretried
    // query already reflects the scan's final state.
    let forward_id = next_id;
    next_id += 1;
    let forward = send_references(
        &client,
        forward_id,
        &my_otui_uri,
        position_of(ui_otui_src, "x"),
        true,
    )
    .expect("forward references present");

    let in_ctrl_lua: Vec<&Location> = forward.iter().filter(|l| l.uri == my_lua_uri).collect();
    assert_eq!(
        in_ctrl_lua.len(),
        1,
        "the associated ctrl.lua's getChildById call must appear exactly once: {forward:#?}"
    );
    assert_eq!(
        in_ctrl_lua[0].range,
        range_of(ctrl_lua_src, "x"),
        "the location must land on the id token inside the quotes"
    );
    assert!(
        forward.iter().all(|l| l.uri != other_lua_uri),
        "an unrelated module's controller (same id, no loadUI association) must never appear: \
         {forward:#?}"
    );
    assert!(
        forward
            .iter()
            .any(|l| l.uri == my_otui_uri && l.range == range_of(ui_otui_src, "x")),
        "the local id: declaration must still be included: {forward:#?}"
    );

    shutdown_and_exit(&client, server_thread, next_id);
}

/// The `/`-rooted (VFS-absolute) `loadUI` half of the module-association bridge (node
/// `bridge-exact-resolution`, commit 1): a controller's `g_ui.loadUI('/modules/othermod/styles/ui')`
/// names its `.otui` by a complete, VFS-absolute literal, resolved against the mounted OTClient
/// virtual filesystem (the detected client root's `mods`/`modules`/`data` overlay, then the bare
/// root — `resolve_vfs_rooted_otui`'s doc comment cites the engine's `resourcemanager.cpp`
/// `resolvePath`) rather than a plain directory join. The target sits in a DIFFERENT module's
/// directory than the controller (`othermod`, not `mymodule`) — the shape neither `paired_uri`'s
/// same-stem fast path nor a plain relative join could ever resolve.
///
/// Mirrors `module_association_pairs_a_controller_with_a_differently_named_and_located_ui_file`'s
/// shape: reverse, polled to convergence via [`references_until`] (module_ui_index is the third,
/// strictly-later scan phase, so "any `Some`" is not a safe readiness signal here — only
/// non-emptiness is, for the same reason that test's doc comment explains).
#[test]
fn vfs_rooted_load_ui_path_pairs_with_a_style_in_a_different_module_directory() {
    let base = std::env::temp_dir().join(format!(
        "otui-vfs-rooted-pairing-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());
    // A real OTClient install root: `init.lua` + `data/` + `modules/` (`mark_as_client_root`) — the
    // mount set a `/`-rooted `loadUI` argument resolves against.
    mark_as_client_root(&base);

    let my_module_dir = base.join("modules").join("mymodule");
    std::fs::create_dir_all(&my_module_dir).expect("mkdir mymodule");
    std::fs::write(
        my_module_dir.join("mymodule.otmod"),
        "Module\n  name: mymodule\n  scripts: [ ctrl ]\n",
    )
    .expect("write mymodule.otmod");
    let ctrl_lua_src = "function onCreate(w)\n  g_ui.loadUI('/modules/othermod/styles/ui')\n  \
                        local btn = w:getChildById('x')\nend\n";
    let ctrl_lua_path = my_module_dir.join("ctrl.lua");
    std::fs::write(&ctrl_lua_path, ctrl_lua_src).expect("write ctrl.lua");

    let other_module_styles_dir = base.join("modules").join("othermod").join("styles");
    std::fs::create_dir_all(&other_module_styles_dir).expect("mkdir othermod/styles");
    let ui_otui_src = "MainWindow < UIWidget\n  Button\n    id: x\n";
    let ui_otui_path = other_module_styles_dir.join("ui.otui");
    std::fs::write(&ui_otui_path, ui_otui_src).expect("write ui.otui");

    let ctrl_lua_uri = file_uri(&ctrl_lua_path);
    let ui_otui_uri = file_uri(&ui_otui_path);

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

    // Open only `ui.otui` — never `ctrl.lua`/`mymodule.otmod` — so the reverse resolution below can
    // only have come from the background disk scan's VFS-rooted module-association resolution.
    client
        .sender
        .send(Message::Notification(Notification::new(
            "textDocument/didOpen".to_owned(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: ui_otui_uri.clone(),
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: ui_otui_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let mut next_id = 2i32;

    // Reverse: ctrl.lua's getChildById('x') -> ui.otui's id: x, resolved purely via the `/`-rooted
    // loadUI path against the detected client root. Polled to convergence (non-empty), not "any
    // Some" — see this test's doc comment.
    let reverse = references_until(
        &client,
        &mut next_id,
        &ctrl_lua_uri,
        position_of(ctrl_lua_src, "x"),
        true,
        |locs: &[Location]| !locs.is_empty(),
    );
    assert_eq!(
        reverse.len(),
        1,
        "exactly one declaration site, resolved via the VFS-rooted loadUI path: {reverse:#?}"
    );
    assert_eq!(reverse[0].uri, ui_otui_uri);
    assert_eq!(reverse[0].range, range_of(ui_otui_src, "x"));

    shutdown_and_exit(&client, server_thread, next_id);
}

/// Negative case for the test above: with NO detected OTClient install root (no `init.lua` +
/// `data`/`modules` siblings anywhere above the module directory), a `/`-rooted `loadUI` argument
/// must NOT pair — silently, never a guess (mirrors `detect_client_roots`'/`resolve_asset_candidates`'
/// existing "no root, no resolution" contract for an ordinary asset path). The exact same layout as
/// the positive test above, minus the client-root markers, PLUS a second, plain-relative `loadUI` in
/// the same controller (`local.otui`/id `y`) that the same `scan_module_dir` call for `mymodule`
/// pairs unconditionally (no client root needed).
///
/// That second pairing is the readiness proof this test needs: `lua_ref_index` is populated by the
/// background scan strictly BEFORE `module_ui_index` (see `references_until`'s doc comment), so
/// `lua_references` answering `Some([])` for the rooted id is not, by itself, distinguishable from
/// "the scan hasn't reached `module_ui_index` for this module yet" — a regression that wrongly added
/// the rooted pairing later would still, in that intermediate window, make this assertion pass. Polling
/// the KNOWN relative pairing to convergence first proves `set_module` has already run for `mymodule`
/// — the very same call that would also have produced the rooted pairing, had one existed — so the
/// direct (unpolled) query for the rooted id right after is checking settled state, not a race.
#[test]
fn vfs_rooted_load_ui_path_does_not_pair_without_a_detected_client_root() {
    let base = std::env::temp_dir().join(format!(
        "otui-vfs-rooted-no-root-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());
    // Deliberately NOT `mark_as_client_root(&base)`: no `init.lua`, no `data/` — so no ancestor walk
    // from the module directory ever finds a client root.

    let my_module_dir = base.join("modules").join("mymodule");
    std::fs::create_dir_all(&my_module_dir).expect("mkdir mymodule");
    std::fs::write(
        my_module_dir.join("mymodule.otmod"),
        "Module\n  name: mymodule\n  scripts: [ ctrl ]\n",
    )
    .expect("write mymodule.otmod");
    let ctrl_lua_src = "function onCreate(w)\n  g_ui.loadUI('/modules/othermod/styles/ui')\n  \
                        g_ui.loadUI('local')\n  \
                        local btn = w:getChildById('x')\n  \
                        local known = w:getChildById('z')\nend\n";
    let ctrl_lua_path = my_module_dir.join("ctrl.lua");
    std::fs::write(&ctrl_lua_path, ctrl_lua_src).expect("write ctrl.lua");

    let other_module_styles_dir = base.join("modules").join("othermod").join("styles");
    std::fs::create_dir_all(&other_module_styles_dir).expect("mkdir othermod/styles");
    let ui_otui_src = "MainWindow < UIWidget\n  Button\n    id: x\n";
    let ui_otui_path = other_module_styles_dir.join("ui.otui");
    // The target genuinely exists on disk — proves the negative result comes from "no client root
    // to resolve against", not "the file happens to be missing".
    std::fs::write(&ui_otui_path, ui_otui_src).expect("write ui.otui");

    // The known, plain-relative pairing, resolved from `ctrl.lua`'s own directory — no client root
    // needed, so `scan_module_dir` always produces it once it runs for `mymodule`.
    let local_otui_src = "MainWindow < UIWidget\n  Button\n    id: z\n";
    let local_otui_path = my_module_dir.join("local.otui");
    std::fs::write(&local_otui_path, local_otui_src).expect("write local.otui");

    let ctrl_lua_uri = file_uri(&ctrl_lua_path);
    let local_otui_uri = file_uri(&local_otui_path);

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

    let mut next_id = 2i32;

    // Readiness proof: poll the KNOWN relative pairing to convergence (non-empty) — see this test's
    // doc comment for why "any `Some`" would not be a safe signal for the rooted query below.
    let known = references_until(
        &client,
        &mut next_id,
        &ctrl_lua_uri,
        position_of(ctrl_lua_src, "z"),
        true,
        |locs: &[Location]| !locs.is_empty(),
    );
    assert_eq!(
        known.len(),
        1,
        "the plain-relative pairing must resolve once module_ui_index has run for mymodule: {known:#?}"
    );
    assert_eq!(known[0].uri, local_otui_uri);

    // Now that `set_module` has run for `mymodule` (proven above), the rooted query is checked once,
    // unpolled: a `Some([])` here is settled state, not an intermediate scan window.
    let reverse = send_references(
        &client,
        next_id,
        &ctrl_lua_uri,
        position_of(ctrl_lua_src, "x"),
        true,
    )
    .expect("references for the rooted id");
    next_id += 1;
    assert!(
        reverse.is_empty(),
        "a /-rooted loadUI path must never pair without a detected client root: {reverse:#?}"
    );

    shutdown_and_exit(&client, server_thread, next_id);
}

/// The reverse bridge (`lua_references`) must honor `ReferenceContext::include_declaration` the same
/// way `collect_references` already does for the OTUI-local `Id` case (spec §5.4): a `getChildById`
/// reference's candidate resolutions — the paired `.otui`'s `id:` declaration AND this same `.lua`
/// document's own `setId(...)` call — are both DECLARATION sites (an `id:` and a `setId` equally
/// *define* the id), so `include_declaration = false` must suppress both, not just one or neither.
/// This id is deliberately declared BOTH ways (`.otui id: closeButton` and Lua `setId('closeButton')`)
/// so the assertion exercises both candidate sources at once, unlike
/// `reverse_references_resolve_a_set_id_call_in_the_same_lua_document`'s Lua-only id.
#[test]
fn reverse_references_honor_include_declaration() {
    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: closeButton\n";
    let panel_otui_uri = Uri::from_str("file:///scratch/include-decl/panel.otui").expect("uri");
    let panel_lua_uri = Uri::from_str("file:///scratch/include-decl/panel.lua").expect("uri");
    let panel_lua_src = "function onCreate(w)\n  \
                          local button = g_ui.createWidget('Button', w)\n  \
                          button:setId('closeButton')\n  \
                          local btn = w:getChildById('closeButton')\nend\n";

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
                    text: panel_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    // Cursor on the getChildById('closeButton') reference (the LAST occurrence — `ref_at` does not
    // recognize the setId literal itself as a reference form).
    let position = position_of_last(panel_lua_src, "closeButton");

    // include_declaration = false: both declaration-class candidates are suppressed.
    let excluded = send_references(&client, 2, &panel_lua_uri, position, false)
        .expect("references present (empty, not null)");
    assert!(
        excluded.is_empty(),
        "include_declaration = false must suppress both the .otui id: and the setId declaration \
         sites: {excluded:#?}"
    );

    // include_declaration = true: both declaration-class candidates are present.
    let included =
        send_references(&client, 3, &panel_lua_uri, position, true).expect("references present");
    assert_eq!(
        included.len(),
        2,
        "include_declaration = true must surface both the .otui id: declaration and the setId \
         declaration: {included:#?}"
    );
    assert!(
        included
            .iter()
            .any(|loc| loc.uri == panel_otui_uri
                && loc.range == range_of(panel_otui_src, "closeButton")),
        "missing the .otui id: declaration site: {included:#?}"
    );
    assert!(
        included
            .iter()
            .any(|loc| loc.uri == panel_lua_uri
                && loc.range == range_of(panel_lua_src, "closeButton")),
        "missing the setId declaration site (the FIRST occurrence, its own definition): {included:#?}"
    );

    shutdown_and_exit(&client, server_thread, 4);
}

/// The reverse bridge must ALSO resolve a `getChildById` reference against a `setId` call **in the
/// same `.lua` document** (node `bridge-exact-resolution`, commit 2) — the id's real, runtime
/// declaration site for a widget created and id'd purely in Lua, which has no `.otui id:` at all.
/// The paired `.otui` here deliberately does NOT declare `bidButton` (the real-corpus shape:
/// `game_cyclopedia/tab/house/house.lua`'s `button:setId('bidButton')`), so any resolution found
/// must have come from the `setId` scan, not `visible_ids`.
///
/// No workspace root at all (mirrors `forward_references_see_an_unsaved_lua_buffer_edit`): both
/// documents are open buffers, same stem/directory, so `paired_uri`'s fast path alone associates
/// them — no background scan to race against.
#[test]
fn reverse_references_resolve_a_set_id_call_in_the_same_lua_document() {
    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: closeButton\n";
    let panel_otui_uri = Uri::from_str("file:///scratch/set-id/panel.otui").expect("uri");
    let panel_lua_uri = Uri::from_str("file:///scratch/set-id/panel.lua").expect("uri");
    let panel_lua_src = "function onCreate(w)\n  \
                          local button = g_ui.createWidget('Button', w)\n  \
                          button:setId('bidButton')\n  \
                          local btn = w:getChildById('bidButton')\nend\n";

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
                    text: panel_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    // Cursor on the getChildById('bidButton') reference — the LAST "bidButton" occurrence (the
    // FIRST is the setId definition itself, which `ref_at` does not recognize as a reference form).
    let reverse = send_references(
        &client,
        2,
        &panel_lua_uri,
        position_of_last(panel_lua_src, "bidButton"),
        true,
    )
    .expect("references present");
    assert_eq!(
        reverse.len(),
        1,
        "exactly one declaration site, resolved via the same-document setId call: {reverse:#?}"
    );
    assert_eq!(reverse[0].uri, panel_lua_uri);
    assert_eq!(
        reverse[0].range,
        range_of(panel_lua_src, "bidButton"),
        "must land on the setId literal (the FIRST occurrence — its own definition site)"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// `textDocument/definition` originating in a `.lua` document (spec §5.3): the cursor on a
/// `controller.ui.<id>` dotted-chain segment must jump to the `id:` declaration in the paired
/// `.otui`. Exercises `Backend::lua_definition` / `resolve_lua_id_declarations` — the only NEW
/// surface this node adds; the resolution pipeline itself is exactly what
/// `reverse_references_honor_include_declaration` already exercises via `textDocument/references`.
#[test]
fn lua_definition_resolves_a_dot_ui_chain_segment_to_the_paired_otui_declaration() {
    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: closeButton\n";
    let panel_otui_uri = Uri::from_str("file:///scratch/lua-def/panel.otui").expect("uri");
    let panel_lua_uri = Uri::from_str("file:///scratch/lua-def/panel.lua").expect("uri");
    let panel_lua_src = "function onCreate(controller)\n  \
                          controller.ui.closeButton:setText('x')\nend\n";

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
                    text: panel_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    let resp = send_definition(
        &client,
        2,
        &panel_lua_uri,
        position_of(panel_lua_src, "closeButton"),
    )
    .expect("definition present");
    match resp {
        GotoDefinitionResponse::Scalar(loc) => {
            assert_eq!(loc.uri, panel_otui_uri);
            assert_eq!(loc.range, range_of(panel_otui_src, "closeButton"));
        }
        other => panic!("expected a single Scalar location: {other:?}"),
    }

    shutdown_and_exit(&client, server_thread, 3);
}

/// [`lua_definition_resolves_a_dot_ui_chain_segment_to_the_paired_otui_declaration`]'s counterpart
/// for the `getChildById('<id>')` string-argument shape, and for a same-file runtime `setId(...)`
/// declaration — mirrors `reverse_references_resolve_a_set_id_call_in_the_same_lua_document`'s
/// fixture exactly (an id with NO `.otui id:` at all, declared purely via `setId` in Lua), proving
/// go-to-definition covers that source too, not just the `.otui` one.
#[test]
fn lua_definition_resolves_a_get_child_by_id_call_against_a_same_file_set_id() {
    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: closeButton\n";
    let panel_otui_uri = Uri::from_str("file:///scratch/lua-def-setid/panel.otui").expect("uri");
    let panel_lua_uri = Uri::from_str("file:///scratch/lua-def-setid/panel.lua").expect("uri");
    let panel_lua_src = "function onCreate(w)\n  \
                          local button = g_ui.createWidget('Button', w)\n  \
                          button:setId('bidButton')\n  \
                          local btn = w:getChildById('bidButton')\nend\n";

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
                    text: panel_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    // Cursor on the getChildById('bidButton') reference — the LAST "bidButton" occurrence (the
    // FIRST is the setId call itself, its own definition site).
    let resp = send_definition(
        &client,
        2,
        &panel_lua_uri,
        position_of_last(panel_lua_src, "bidButton"),
    )
    .expect("definition present");
    match resp {
        GotoDefinitionResponse::Scalar(loc) => {
            assert_eq!(loc.uri, panel_lua_uri);
            assert_eq!(
                loc.range,
                range_of(panel_lua_src, "bidButton"),
                "must land on the setId literal (the FIRST occurrence — its own definition site)"
            );
        }
        other => panic!("expected a single Scalar location: {other:?}"),
    }

    shutdown_and_exit(&client, server_thread, 3);
}

/// A `getChildById` call whose id has no `.otui` declaration anywhere and no same-file `setId` must
/// resolve to nothing — `textDocument/definition`'s "no target" answer is an absent result, exactly
/// like a native base in the OTUI-local case ([`resolve_base_definition`]).
#[test]
fn lua_definition_for_an_unresolved_id_is_none() {
    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: closeButton\n";
    let panel_otui_uri = Uri::from_str("file:///scratch/lua-def-missing/panel.otui").expect("uri");
    let panel_lua_uri = Uri::from_str("file:///scratch/lua-def-missing/panel.lua").expect("uri");
    let panel_lua_src = "function onCreate(w)\n  local btn = w:getChildById('doesNotExist')\nend\n";

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
                    text: panel_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    let resp = send_definition(
        &client,
        2,
        &panel_lua_uri,
        position_of(panel_lua_src, "doesNotExist"),
    );
    assert!(
        resp.is_none(),
        "an id with no .otui declaration and no local setId must resolve to nothing: {resp:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Regression: extracting `resolve_lua_id_declarations` out of `lua_references` must not change
/// `textDocument/references`'s own answer. Re-runs
/// `reverse_references_honor_include_declaration`'s exact fixture and both `include_declaration`
/// values, asserting the SAME two-location result the pre-refactor implementation produced.
#[test]
fn references_are_unchanged_by_the_lua_definition_refactor() {
    let panel_otui_src = "Panel < UIWidget\n  Button\n    id: closeButton\n";
    let panel_otui_uri =
        Uri::from_str("file:///scratch/lua-def-regression/panel.otui").expect("uri");
    let panel_lua_uri = Uri::from_str("file:///scratch/lua-def-regression/panel.lua").expect("uri");
    let panel_lua_src = "function onCreate(w)\n  \
                          button:setId('closeButton')\n  \
                          local btn = w:getChildById('closeButton')\nend\n";

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
                    text: panel_lua_src.to_owned(),
                },
            },
        )))
        .expect("send didOpen lua");
    let _ = recv_diagnostics(&client, &panel_lua_uri);

    let cursor = position_of_last(panel_lua_src, "closeButton");

    let excluded =
        send_references(&client, 2, &panel_lua_uri, cursor, false).expect("references present");
    assert!(
        excluded.is_empty(),
        "include_declaration = false must still suppress both declaration sites: {excluded:#?}"
    );

    let included =
        send_references(&client, 3, &panel_lua_uri, cursor, true).expect("references present");
    assert_eq!(
        included.len(),
        2,
        "include_declaration = true must surface both the .otui id: declaration and the setId \
         declaration: {included:#?}"
    );
    assert!(
        included
            .iter()
            .any(|l| l.uri == panel_otui_uri && l.range == range_of(panel_otui_src, "closeButton")),
        "missing the .otui id: declaration: {included:#?}"
    );
    assert!(
        included
            .iter()
            .any(|l| l.uri == panel_lua_uri && l.range == range_of(panel_lua_src, "closeButton")),
        "missing the setId declaration site (the FIRST occurrence, its own definition): {included:#?}"
    );

    shutdown_and_exit(&client, server_thread, 4);
}

/// The module-union fallback ([`Backend::resolve_module_unique_id_declaration`], reached only when
/// exact pairing resolves nothing): three modules, one workspace, one background disk scan, covering
/// all three ranked outcomes plus both directions of the bridge in a single deterministic pass.
///
/// * `scenario_unique/`: `controller.lua` never calls `loadUI` and has no same-stem `.otui` sibling,
///   so exact pairing ([`Backend::associated_uris`]) finds nothing at all for it. The module's ONLY
///   `.otui` declaring `targetId` is a nested sibling (`panels/settings.otui`) the controller never
///   loads — unique in the module, so the fallback resolves it.
/// * `scenario_ambiguous/`: same shape, but the id (`dupId`) is declared in TWO sibling `.otui` files
///   (`panels/a.otui` and `panels/b.otui`). Genuinely ambiguous — the fallback must decline (resolve
///   to nothing) rather than guess between them.
/// * `scenario_exact/`: `controller.lua`/`controller.otui` share a directory AND stem, so the fast
///   path alone already resolves `pairedId` — even though a THIRD file in the same module
///   (`other/decoy.otui`) also declares `pairedId`, making it just as ambiguous at the module level as
///   `scenario_ambiguous`'s id. Exact pairing already succeeding must short-circuit the fallback
///   entirely, so the paired declaration (not "nothing", the fallback's answer to that same ambiguity)
///   is what comes back — proving the ranking in `resolve_lua_id_declarations`.
///
/// Every `.otui`/`.lua` file here is left CLOSED (never `didOpen`) so every result in this test can
/// only have come from the background disk scan — exercising the real `find_owning_module_dir`/
/// `collect_files_under` disk walk the fallback performs, not the open-buffer fast path. Both
/// `textDocument/references` (`include_declaration: true`) and `textDocument/definition` are checked
/// per scenario, proving both share the fallback via `resolve_lua_id_declarations` (spec §5.3/§5.4).
#[test]
fn lua_bridge_module_union_fallback_covers_unique_ambiguous_and_exact_pairing() {
    let base = std::env::temp_dir().join(format!(
        "otui-module-union-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());

    // --- scenario_unique: fallback resolves a sibling `.otui` this controller never loads. ---
    let unique_dir = base.join("modules").join("scenario_unique");
    std::fs::create_dir_all(unique_dir.join("panels")).expect("mkdir scenario_unique/panels");
    std::fs::write(
        unique_dir.join("scenario_unique.otmod"),
        "Module\n  name: scenario_unique\n  scripts: [ controller ]\n",
    )
    .expect("write otmod");
    let unique_lua_src =
        "function onCreate(self)\n  local btn = self.ui.targetId\n  btn:hide()\nend\n";
    std::fs::write(unique_dir.join("controller.lua"), unique_lua_src)
        .expect("write controller.lua");
    let unique_otui_src = "Panel < UIWidget\n  Button\n    id: targetId\n";
    std::fs::write(
        unique_dir.join("panels").join("settings.otui"),
        unique_otui_src,
    )
    .expect("write settings.otui");
    let unique_lua_uri = file_uri(&unique_dir.join("controller.lua"));
    let unique_otui_uri = file_uri(&unique_dir.join("panels").join("settings.otui"));

    // --- scenario_ambiguous: the id is declared in TWO sibling `.otui` files -> declines. ---
    let ambiguous_dir = base.join("modules").join("scenario_ambiguous");
    std::fs::create_dir_all(ambiguous_dir.join("panels")).expect("mkdir scenario_ambiguous/panels");
    std::fs::write(
        ambiguous_dir.join("scenario_ambiguous.otmod"),
        "Module\n  name: scenario_ambiguous\n  scripts: [ controller ]\n",
    )
    .expect("write otmod");
    let ambiguous_lua_src =
        "function onCreate(self)\n  local btn = self.ui.dupId\n  btn:hide()\nend\n";
    std::fs::write(ambiguous_dir.join("controller.lua"), ambiguous_lua_src)
        .expect("write controller.lua");
    let dup_otui_src = "Panel < UIWidget\n  Button\n    id: dupId\n";
    std::fs::write(ambiguous_dir.join("panels").join("a.otui"), dup_otui_src)
        .expect("write a.otui");
    std::fs::write(ambiguous_dir.join("panels").join("b.otui"), dup_otui_src)
        .expect("write b.otui");
    let ambiguous_lua_uri = file_uri(&ambiguous_dir.join("controller.lua"));

    // --- scenario_exact: exact pairing already resolves `pairedId`, even though the module also
    // holds a second, unrelated declaration of it (`other/decoy.otui`) that would otherwise make it
    // just as ambiguous as `scenario_ambiguous`'s id. ---
    let exact_dir = base.join("modules").join("scenario_exact");
    std::fs::create_dir_all(exact_dir.join("other")).expect("mkdir scenario_exact/other");
    std::fs::write(
        exact_dir.join("scenario_exact.otmod"),
        "Module\n  name: scenario_exact\n  scripts: [ controller ]\n",
    )
    .expect("write otmod");
    let exact_lua_src =
        "function onCreate(self)\n  local btn = self.ui.pairedId\n  btn:hide()\nend\n";
    std::fs::write(exact_dir.join("controller.lua"), exact_lua_src).expect("write controller.lua");
    let paired_otui_src = "Panel < UIWidget\n  Button\n    id: pairedId\n";
    std::fs::write(exact_dir.join("controller.otui"), paired_otui_src)
        .expect("write controller.otui");
    std::fs::write(exact_dir.join("other").join("decoy.otui"), paired_otui_src)
        .expect("write decoy.otui");
    let exact_lua_uri = file_uri(&exact_dir.join("controller.lua"));
    let exact_otui_uri = file_uri(&exact_dir.join("controller.otui"));

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

    let mut next_id = 2i32;

    // The `.otui` scan phase (which populates every candidate the fallback reads) runs to
    // completion, in full, strictly BEFORE the `.lua` scan phase starts (see `run_initialized`'s
    // doc comment on the two sequential loops) — so the first `Some` this poll observes for
    // `unique_lua_uri` already proves every `.otui` file in the whole workspace, across all three
    // scenario directories, is indexed. Every other assertion below can then be a single,
    // un-retried call.
    let unique_refs = references_until(
        &client,
        &mut next_id,
        &unique_lua_uri,
        position_of(unique_lua_src, "targetId"),
        true,
        |_locs| true,
    );
    assert_eq!(
        unique_refs,
        vec![Location {
            uri: unique_otui_uri.clone(),
            range: range_of(unique_otui_src, "targetId"),
        }],
        "unique-in-module fallback must resolve the sole declaring sibling .otui: {unique_refs:#?}"
    );

    let unique_def_id = next_id;
    next_id += 1;
    let unique_def = send_definition(
        &client,
        unique_def_id,
        &unique_lua_uri,
        position_of(unique_lua_src, "targetId"),
    )
    .expect("definition present");
    match unique_def {
        GotoDefinitionResponse::Scalar(loc) => {
            assert_eq!(loc.uri, unique_otui_uri);
            assert_eq!(loc.range, range_of(unique_otui_src, "targetId"));
        }
        other => panic!("expected a single Scalar location: {other:?}"),
    }

    // The ambiguous verdict is scan-timing-independent: the fallback reads its candidate `.otui`
    // straight from disk (`collect_files_under`, with the merged-buffer text only as an override),
    // so both duplicate declarers are always seen regardless of index state — a single un-polled call
    // is stable here (unlike the exact case below, which resolves through the merged-`documents`
    // index and so must poll for the scan).
    let ambiguous_refs_id = next_id;
    next_id += 1;
    let ambiguous_refs = send_references(
        &client,
        ambiguous_refs_id,
        &ambiguous_lua_uri,
        position_of(ambiguous_lua_src, "dupId"),
        true,
    )
    .expect("references present (Some(empty), not None: the cursor IS on a recognized ref)");
    assert!(
        ambiguous_refs.is_empty(),
        "an id declared in two sibling .otui files must decline, not guess: {ambiguous_refs:#?}"
    );

    let ambiguous_def_id = next_id;
    next_id += 1;
    let ambiguous_def = send_definition(
        &client,
        ambiguous_def_id,
        &ambiguous_lua_uri,
        position_of(ambiguous_lua_src, "dupId"),
    );
    assert!(
        ambiguous_def.is_none(),
        "an ambiguous module-wide id must resolve to nothing: {ambiguous_def:?}"
    );

    // Exact pairing resolves through the merged-`documents` index, so this path IS scan-timing
    // dependent — poll until the background scan has indexed the paired `.otui`, rather than relying
    // on filesystem scan order.
    let exact_refs = references_until(
        &client,
        &mut next_id,
        &exact_lua_uri,
        position_of(exact_lua_src, "pairedId"),
        true,
        |locs| !locs.is_empty(),
    );
    assert_eq!(
        exact_refs,
        vec![Location {
            uri: exact_otui_uri.clone(),
            range: range_of(paired_otui_src, "pairedId"),
        }],
        "exact pairing must win outright — the fallback's own ambiguity verdict (nothing) must never \
         override an already-successful exact pairing: {exact_refs:#?}"
    );

    // Same module, already proven indexed by the poll above — a single call is stable now.
    let exact_def_id = next_id;
    next_id += 1;
    let exact_def = send_definition(
        &client,
        exact_def_id,
        &exact_lua_uri,
        position_of(exact_lua_src, "pairedId"),
    )
    .expect("definition present");
    match exact_def {
        GotoDefinitionResponse::Scalar(loc) => {
            assert_eq!(loc.uri, exact_otui_uri);
            assert_eq!(loc.range, range_of(paired_otui_src, "pairedId"));
        }
        other => panic!("expected a single Scalar location: {other:?}"),
    }

    shutdown_and_exit(&client, server_thread, next_id);
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

    let mod_lua_src = "function onCreate(rootWidget)\n  local btn = rootWidget:getChildById('closeButton')\nend\n";
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

    // Poll until mod.lua is indexed (see `references_until`'s doc comment): the background scan
    // indexes every `.otui` file (base.otui's `MiniWindow` def, into `style_index`) BEFORE it indexes
    // any `.lua` file (mod.lua's getChildById call, into `lua_ref_index`/`lua_texts`) — a single
    // sequential background thread runs the `.otui` scan to completion, then the `.lua` scan — so the
    // first `Some` response here (mod.lua indexed) already guarantees base.otui was indexed too. The
    // predicate only waits for readiness, not the expected content, so a real resolution bug fails via
    // the `assert_eq!`s below, not this poll's timeout.
    let mut next_id = 2i32;
    let reverse = references_until(
        &client,
        &mut next_id,
        &mod_lua_uri,
        position_of(mod_lua_src, "closeButton"),
        true,
        |_locs| true,
    );

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

    shutdown_and_exit(&client, server_thread, next_id);
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

/// A `/`-rooted `loadUI` target can live in a DIFFERENT module directory than its controller
/// (`vfs_rooted_load_ui_path_pairs_with_a_style_in_a_different_module_directory`). This test proves
/// `update_module_index_for` keeps that pairing FRESH when the target itself is created or deleted
/// after the initial scan — not just re-derived from whichever module owns the changed file (that
/// scoped rebuild would silently miss a cross-module rooted target, per this node's Finding 3): a
/// watched `.otui` `CREATED`/`DELETED` event triggers a full [`build_module_index`] rebuild instead.
///
/// The controller's module also declares a plain-relative `loadUI('local')`/id `z` pairing, present
/// from the start — the SAME readiness anchor `vfs_rooted_load_ui_path_does_not_pair_without_a_
/// detected_client_root` uses: polling it to convergence proves the ONE-TIME background scan has
/// already written `module_ui_index` once, after which every further mutation is this test's own
/// synchronous `workspace/didChangeWatchedFiles` notification, processed by the single-threaded
/// dispatch loop strictly before the next request — so every assertion after that point is a single,
/// unpolled query on settled state, never a race.
#[test]
fn watched_otui_create_and_delete_refreshes_a_cross_module_rooted_pairing() {
    let base = std::env::temp_dir().join(format!(
        "otui-vfs-rooted-watch-{}-{}",
        std::process::id(),
        line!()
    ));
    let _cleanup = TempDirGuard(base.clone());
    mark_as_client_root(&base);

    let my_module_dir = base.join("modules").join("mymodule");
    std::fs::create_dir_all(&my_module_dir).expect("mkdir mymodule");
    std::fs::write(
        my_module_dir.join("mymodule.otmod"),
        "Module\n  name: mymodule\n  scripts: [ ctrl ]\n",
    )
    .expect("write mymodule.otmod");
    let ctrl_lua_src = "function onCreate(w)\n  g_ui.loadUI('/modules/othermod/styles/ui')\n  \
                        g_ui.loadUI('local')\n  \
                        local btn = w:getChildById('x')\n  \
                        local known = w:getChildById('z')\nend\n";
    let ctrl_lua_path = my_module_dir.join("ctrl.lua");
    std::fs::write(&ctrl_lua_path, ctrl_lua_src).expect("write ctrl.lua");

    let local_otui_src = "MainWindow < UIWidget\n  Button\n    id: z\n";
    std::fs::write(my_module_dir.join("local.otui"), local_otui_src).expect("write local.otui");

    let other_module_styles_dir = base.join("modules").join("othermod").join("styles");
    std::fs::create_dir_all(&other_module_styles_dir).expect("mkdir othermod/styles");
    let ui_otui_src = "MainWindow < UIWidget\n  Button\n    id: x\n";
    let ui_otui_path = other_module_styles_dir.join("ui.otui");
    // Deliberately not written yet: the rooted target does not exist at initial-scan time.

    let ctrl_lua_uri = file_uri(&ctrl_lua_path);
    let ui_otui_uri = file_uri(&ui_otui_path);

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

    let mut next_id = 2i32;

    // Readiness anchor: poll the known relative pairing to convergence (see this test's doc
    // comment for why this makes every later assertion a single, unpolled, race-free query).
    let known = references_until(
        &client,
        &mut next_id,
        &ctrl_lua_uri,
        position_of(ctrl_lua_src, "z"),
        true,
        |locs: &[Location]| !locs.is_empty(),
    );
    assert_eq!(known.len(), 1, "readiness anchor must resolve: {known:#?}");

    // Before creation: the rooted target does not exist on disk yet, so no pairing.
    let before = send_references(
        &client,
        next_id,
        &ctrl_lua_uri,
        position_of(ctrl_lua_src, "x"),
        true,
    )
    .expect("references present (empty, not null)");
    next_id += 1;
    assert!(
        before.is_empty(),
        "the rooted target does not exist yet; must not pair: {before:#?}"
    );

    // Create the rooted target on disk, then deliver the watched CREATED event.
    std::fs::write(&ui_otui_path, ui_otui_src).expect("write ui.otui");
    client
        .sender
        .send(Message::Notification(Notification::new(
            "workspace/didChangeWatchedFiles".to_owned(),
            DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: ui_otui_uri.clone(),
                    typ: FileChangeType::CREATED,
                }],
            },
        )))
        .expect("send didChangeWatchedFiles created");

    let after_create = send_references(
        &client,
        next_id,
        &ctrl_lua_uri,
        position_of(ctrl_lua_src, "x"),
        true,
    )
    .expect("references present");
    next_id += 1;
    assert_eq!(
        after_create.len(),
        1,
        "creating the rooted target must refresh the cross-module pairing: {after_create:#?}"
    );
    assert_eq!(after_create[0].uri, ui_otui_uri);
    assert_eq!(after_create[0].range, range_of(ui_otui_src, "x"));

    // Delete the rooted target, then deliver the watched DELETED event.
    std::fs::remove_file(&ui_otui_path).expect("remove ui.otui");
    client
        .sender
        .send(Message::Notification(Notification::new(
            "workspace/didChangeWatchedFiles".to_owned(),
            DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: ui_otui_uri.clone(),
                    typ: FileChangeType::DELETED,
                }],
            },
        )))
        .expect("send didChangeWatchedFiles deleted");

    let after_delete = send_references(
        &client,
        next_id,
        &ctrl_lua_uri,
        position_of(ctrl_lua_src, "x"),
        true,
    )
    .expect("references present (empty, not null)");
    next_id += 1;
    assert!(
        after_delete.is_empty(),
        "deleting the rooted target must clear the stale cross-module pairing: {after_delete:#?}"
    );

    shutdown_and_exit(&client, server_thread, next_id);
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

/// The regression this whole node exists to fix: opening a real-shaped `.otmod` module manifest
/// must never publish a widget `unknown-property` diagnostic against its manifest keys
/// (`name:`/`description:`/`scripts:`/`sandboxed:`/`@onLoad:`, …) — those are manifest metadata,
/// not widget style properties, and the widget catalog was never meant to judge them (spec: this
/// crate's `otui_core::manifest`, ground-truthed against `module.cpp`'s `Module::discover`).
#[test]
fn otmod_didopen_publishes_no_widget_unknown_property_diagnostics() {
    let uri = Uri::from_str("file:///scratch/game_shop.otmod").expect("uri");
    let source = "\
Module
  name: game_shop
  description: In-game shop
  author: someone
  website: https://example.invalid
  sandboxed: true
  scripts: [ game_shop ]
  @onLoad: init()
  @onUnload: terminate()
";

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
                    // The client may send any of "otui"/"otml"/its own id for a `.otmod`, exactly
                    // as it already does for a `.otui` (see `Language::classify`'s doc comment) —
                    // the `.otmod` extension alone must be enough to route this correctly.
                    language_id: "otui".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let published = recv_diagnostics(&client, &uri);
    assert!(
        published
            .diagnostics
            .iter()
            .all(|d| d.code != Some(NumberOrString::String("unknown-property".to_owned()))),
        "a widget unknown-property diagnostic must never fire on a module manifest: {:#?}",
        published.diagnostics
    );
    // Every key in this manifest is one `Module::discover` actually reads, so the well-formed
    // manifest yields no diagnostics at all — not even a manifest-schema hint.
    assert!(
        published.diagnostics.is_empty(),
        "a well-formed manifest should have no diagnostics: {:#?}",
        published.diagnostics
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// A manifest key the engine never reads (`minClientVersion:` — observed verbatim in four real
/// OTClient module manifests, none of which read it in `module.cpp` either) is a
/// `unknown-manifest-key` **Hint**, never an Error and never a widget `unknown-property` — spec
/// §2.10's posture, end to end through the real publish path.
#[test]
fn otmod_unknown_manifest_key_is_a_hint_not_an_error() {
    let uri = Uri::from_str("file:///scratch/inspect.otmod").expect("uri");
    let source = "Module\n  name: game_inspect\n  minClientVersion: 1511\n";

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

    let published = recv_diagnostics(&client, &uri);
    assert_eq!(
        published.diagnostics.len(),
        1,
        "exactly one finding, the unknown key: {:#?}",
        published.diagnostics
    );
    let diag = &published.diagnostics[0];
    assert_eq!(diag.severity, Some(DiagnosticSeverity::HINT));
    assert_eq!(
        diag.code,
        Some(NumberOrString::String("unknown-manifest-key".to_owned()))
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// Only the DIAGNOSTICS path changes for a `.otmod`: the purely syntactic surfaces (semantic
/// tokens, folding) still run — a module manifest is OTML, and both operate on the shared grammar
/// alone, never the widget-vs-manifest schema.
#[test]
fn semantic_tokens_and_folding_still_serve_a_otmod_document() {
    let uri = Uri::from_str("file:///scratch/topmenu.otmod").expect("uri");
    // A multi-line `@onLoad:` block scalar body gives folding something multi-line to collapse,
    // exactly like the widget-`.otui` folding tests do for a block scalar.
    let source = "\
Module
  name: client_topmenu
  scripts: [ topmenu ]
  @onLoad: |
    init()
    connect(g_game, { onGameStart = online })
";

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
            "textDocument/semanticTokens/full".to_owned(),
            lsp_types::SemanticTokensParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        )))
        .expect("send semanticTokens/full");
    let resp = recv_response(&client, &RequestId::from(2));
    assert!(resp.error.is_none(), "semanticTokens errored: {resp:?}");
    let tokens: lsp_types::SemanticTokensResult =
        serde_json::from_value(resp.result.expect("semanticTokens result present"))
            .expect("deserialize SemanticTokensResult");
    let lsp_types::SemanticTokensResult::Tokens(tokens) = tokens else {
        panic!("expected the Tokens variant: {tokens:?}");
    };
    assert!(
        !tokens.data.is_empty(),
        "semantic tokens must still be produced for a .otmod document"
    );

    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(3),
            "textDocument/foldingRange".to_owned(),
            lsp_types::FoldingRangeParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        )))
        .expect("send foldingRange");
    let resp = recv_response(&client, &RequestId::from(3));
    assert!(resp.error.is_none(), "foldingRange errored: {resp:?}");
    let folds: Vec<lsp_types::FoldingRange> =
        serde_json::from_value(resp.result.expect("foldingRange result present"))
            .expect("deserialize Vec<FoldingRange>");
    assert!(
        !folds.is_empty(),
        "the multi-line @onLoad block scalar body must still fold on a .otmod document"
    );

    shutdown_and_exit(&client, server_thread, 4);
}

/// A `.otfont` document is judged against the font-manifest schema, not the module one — `texture:`/
/// `glyph-size:`/`height:` are real font keys, and neither must fire a widget `unknown-property` nor
/// a `.otmod`-flavored diagnostic.
#[test]
fn otfont_didopen_publishes_no_widget_unknown_property_diagnostics() {
    let uri = Uri::from_str("file:///scratch/small-9px.otfont").expect("uri");
    // `data/fonts/otfont/small-9px.otfont`-style real shape.
    let source = "\
Font
  name: small-9px
  texture: small-9px
  height: 9
  glyph-size: 9 9
  space-width: 3
  spacing: 1 0
";

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

    let published = recv_diagnostics(&client, &uri);
    assert!(
        published.diagnostics.is_empty(),
        "a well-formed font manifest should have no diagnostics: {:#?}",
        published.diagnostics
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// The schema selector must agree with the classifier on every URI form — not just the `file:`
/// extension form the previous test already covers. Here the URI carries no `.otfont` extension at
/// all (so a `file:`-URI-extension-only schema check would miss it entirely), and only the
/// `didOpen` `languageId` says "this is a font manifest" — exactly the signal
/// `Language::classify`/`Language::from_uri` already honor on their own (see their doc comments).
///
/// Before this was fixed, the schema picker used a second, narrower, `file:`-URI-only check that
/// disagreed with the classifier in exactly this case: it saw no `.otfont` extension, fell back to
/// the module schema, and the well-formed font manifest below would have been wrongly flagged with
/// `missing-module-root` (no top-level `Module` node — because this document's root is `Font`).
#[test]
fn otfont_recognized_only_by_language_id_still_uses_font_schema() {
    let uri = Uri::from_str("file:///scratch/small-9px.fontdata").expect("uri");
    let source = "\
Font
  name: small-9px
  texture: small-9px
  height: 9
  glyph-size: 9 9
  space-width: 3
  spacing: 1 0
";

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
                    language_id: "otfont".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    let published = recv_diagnostics(&client, &uri);
    assert!(
        !published
            .diagnostics
            .iter()
            .any(|d| d.code == Some(NumberOrString::String("missing-module-root".to_owned()))),
        "a font manifest recognized only via languageId must never be judged against the module \
         schema: {:#?}",
        published.diagnostics
    );
    assert!(
        published.diagnostics.is_empty(),
        "a well-formed font manifest should have no diagnostics, whatever URI form named it: {:#?}",
        published.diagnostics
    );

    shutdown_and_exit(&client, server_thread, 2);
}

/// `textDocument/codeAction` must still serve a `.otmod` module manifest: it is OTML syntactically
/// (see [`semantic_tokens_and_folding_still_serve_a_otmod_document`]), so the tabs→spaces quick-fix
/// that corrects a `tab-indentation` diagnostic applies to it exactly as it does to a widget
/// `.otui` — the parse-level indentation rule is the OTML *parser*'s own, not the widget style
/// resolver's. This is the regression `Backend::code_action` routing through `otml_document_text`
/// (rather than the OTUI-only `otui_document_text`) exists to fix: before it, a manifest's own
/// `did_open`/`did_change` diagnostics already flagged its `tab-indentation` mistake (spec:
/// `structural_diagnostics`, shared by every OTML document), but no `codeAction` request could ever
/// reach `build_manifest_code_actions` to offer the matching fix.
#[test]
fn otmod_tab_indentation_offers_the_tabs_to_spaces_quick_fix() {
    let uri = Uri::from_str("file:///scratch/tabbed.otmod").expect("uri");
    let source = "Module\n\tname: tabbed\n  scripts: [ tabbed ]\n";

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
                    language_id: "otmod".to_owned(),
                    version: 1,
                    text: source.to_owned(),
                },
            },
        )))
        .expect("send didOpen");

    // The published diagnostics confirm the manifest path (not the widget-aware one) is what ran:
    // a `tab-indentation` finding, but never a widget `unknown-property` hint for `name:`/`scripts:`.
    let published = recv_diagnostics(&client, &uri);
    assert!(
        published
            .diagnostics
            .iter()
            .any(|d| d.code == Some(NumberOrString::String("tab-indentation".to_owned()))),
        "expected a tab-indentation diagnostic: {:#?}",
        published.diagnostics
    );
    assert!(
        !published
            .diagnostics
            .iter()
            .any(|d| d.code == Some(NumberOrString::String("unknown-property".to_owned()))),
        "a .otmod must never surface a widget unknown-property diagnostic: {:#?}",
        published.diagnostics
    );

    // Request a code action over the tab-indented line.
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(2),
            "textDocument/codeAction".to_owned(),
            CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: range_of(source, "name"),
                context: CodeActionContext::default(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
        )))
        .expect("send codeAction");
    let resp = recv_response(&client, &RequestId::from(2));
    assert!(resp.error.is_none(), "codeAction errored: {resp:?}");
    let actions: Vec<CodeActionOrCommand> =
        serde_json::from_value(resp.result.expect("codeAction result present"))
            .expect("deserialize Vec<CodeActionOrCommand>");

    let titles: Vec<String> = actions
        .iter()
        .map(|a| match a {
            CodeActionOrCommand::CodeAction(action) => action.title.clone(),
            CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
        })
        .collect();
    assert!(
        titles.iter().any(|t| t == "Convert tabs to spaces"),
        "expected the tabs-to-spaces quick-fix on a .otmod document, got: {titles:?}"
    );

    // Revert-confirm: applying the fix's own edit turns the tab back into two spaces, and
    // re-running code_action on the fixed text over the same line offers nothing more to fix.
    let CodeActionOrCommand::CodeAction(action) = actions
        .into_iter()
        .find(|a| matches!(a, CodeActionOrCommand::CodeAction(a) if a.title == "Convert tabs to spaces"))
        .expect("the tab fix is present")
    else {
        unreachable!("matched above");
    };
    let changes = action
        .edit
        .as_ref()
        .expect("the tab fix carries a workspace edit")
        .changes
        .as_ref()
        .expect("the tab fix carries changes");
    assert_eq!(changes.len(), 1, "one document is edited");
    let (edited_uri, edits) = changes.iter().next().expect("one entry");
    assert_eq!(*edited_uri, uri, "the tab fix edits this document");
    assert_eq!(edits.len(), 1, "exactly one edit corrects the tab line");
    assert_eq!(edits[0].new_text, "  ", "a leading tab becomes two spaces");
    assert_eq!(edits[0].range.start, Position::new(1, 0));
    assert_eq!(edits[0].range.end, Position::new(1, 1));

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hover on a **dotted** anchor target id (`anchors.top: header.bottom`) that names a direct sibling
/// (spec §5.5): the hover shows the resolved sibling's own widget kind, not just the id text.
#[test]
fn hover_on_a_sibling_anchor_target_shows_the_sibling_widget_kind() {
    let uri = Uri::from_str("file:///scratch/anchor-hover-sibling.otui").expect("uri");
    let source = "MainWindow < UIWidget\n  Header < UILabel\n    id: header\n  \
        Body < UIWidget\n    anchors.top: header.bottom\n";

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

    let resp = send_hover(&client, 2, &uri, position_of(source, "header.bottom"));
    let value = hover_markdown(&resp);
    assert!(
        value.contains("header"),
        "expected the id in the hover: {value:?}"
    );
    assert!(
        value.contains("UILabel"),
        "expected the resolved sibling's widget kind in the hover: {value:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hover on the bare `anchors.fill:`/`anchors.centerIn:` shorthand target — the shape `id_at` cannot
/// classify (no `.edge` suffix at all) but `resolve_anchor_target` must still resolve.
#[test]
fn hover_on_a_bare_fill_shorthand_target_shows_the_sibling_widget_kind() {
    let uri = Uri::from_str("file:///scratch/anchor-hover-bare-fill.otui").expect("uri");
    let source = "MainWindow < UIWidget\n  Header < UILabel\n    id: header\n  \
        Body < UIWidget\n    anchors.fill: header\n";

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

    // The last `header` occurrence is the `anchors.fill:` value, not the `id: header` declaration.
    let resp = send_hover(&client, 2, &uri, position_of_last(source, "header"));
    let value = hover_markdown(&resp);
    assert!(
        value.contains("UILabel"),
        "expected the resolved sibling's widget kind in the hover: {value:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Hover on each magic anchor pseudo-target (`parent`/`next`/`prev`) shows a short explanation of the
/// keyword, never an id lookup.
#[test]
fn hover_on_magic_anchor_targets_shows_the_pseudo_target_explanation() {
    let uri = Uri::from_str("file:///scratch/anchor-hover-magic.otui").expect("uri");
    let source = "MainWindow < UIWidget\n  A < UIWidget\n    anchors.top: parent.top\n  \
        B < UIWidget\n    anchors.top: prev.bottom\n  \
        C < UIWidget\n    anchors.top: next.bottom\n";

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

    let parent_value = hover_markdown(&send_hover(
        &client,
        2,
        &uri,
        position_of(source, "parent.top"),
    ));
    assert!(parent_value.contains("parent"), "{parent_value:?}");
    assert!(parent_value.contains("parent widget"), "{parent_value:?}");

    let prev_value = hover_markdown(&send_hover(
        &client,
        3,
        &uri,
        position_of(source, "prev.bottom"),
    ));
    assert!(prev_value.contains("previous sibling"), "{prev_value:?}");

    let next_value = hover_markdown(&send_hover(
        &client,
        4,
        &uri,
        position_of(source, "next.bottom"),
    ));
    assert!(next_value.contains("next sibling"), "{next_value:?}");

    shutdown_and_exit(&client, server_thread, 5);
}

/// Hover on an anchor target id that is not a magic keyword and does not name any direct sibling (an
/// ancestor's own id here) says "not found" — the engine's `getChildById` lookup would silently drop
/// it too, never a diagnostic.
#[test]
fn hover_on_a_non_sibling_anchor_target_id_says_not_found() {
    let uri = Uri::from_str("file:///scratch/anchor-hover-unresolved.otui").expect("uri");
    let source = "MainWindow < UIWidget\n  id: outer\n  Body < UIWidget\n    anchors.fill: outer\n";

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

    // The last `outer` occurrence is the anchor target, not the `id: outer` declaration.
    let resp = send_hover(&client, 2, &uri, position_of_last(source, "outer"));
    let value = hover_markdown(&resp);
    assert!(
        value.contains("not found"),
        "an ancestor's id must not resolve, and must say so: {value:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Go-to-definition on an anchor target id that names a direct sibling jumps to that sibling's own
/// `id:` declaration, in the same file (spec §5.3).
#[test]
fn goto_definition_on_a_sibling_anchor_target_jumps_to_its_id_declaration() {
    let uri = Uri::from_str("file:///scratch/anchor-goto-sibling.otui").expect("uri");
    let source = "MainWindow < UIWidget\n  Header < UILabel\n    id: header\n  \
        Body < UIWidget\n    anchors.top: header.bottom\n";

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

    let resp = send_goto_definition(&client, 2, &uri, position_of(source, "header.bottom"))
        .expect("a direct sibling must resolve to a definition");
    let GotoDefinitionResponse::Scalar(location) = resp else {
        panic!("expected a single Location, got {resp:?}");
    };
    assert_eq!(location.uri, uri, "the jump stays in the same file");
    assert_eq!(
        location.range,
        range_of(source, "header"),
        "the jump lands on the sibling's own `id: header` declaration, not the anchor target"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Go-to-definition on a magic anchor target (`parent`/`next`/`prev`) has nothing to jump to — it is
/// not a user id at all.
#[test]
fn goto_definition_on_a_magic_anchor_target_is_none() {
    let uri = Uri::from_str("file:///scratch/anchor-goto-magic.otui").expect("uri");
    let source = "MainWindow < UIWidget\n  Body < UIWidget\n    anchors.fill: parent\n";

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

    let resp = send_goto_definition(&client, 2, &uri, position_of(source, "parent"));
    assert!(
        resp.is_none(),
        "a magic target has no id to jump to: {resp:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}

/// Go-to-definition on an anchor target id that does not name any direct sibling (an ancestor's own
/// id) is `None` — the engine's `getChildById` would never resolve it, so jumping anywhere would
/// mislead the author.
#[test]
fn goto_definition_on_a_non_sibling_anchor_target_id_is_none() {
    let uri = Uri::from_str("file:///scratch/anchor-goto-unresolved.otui").expect("uri");
    let source = "MainWindow < UIWidget\n  id: outer\n  Body < UIWidget\n    anchors.fill: outer\n";

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

    // The last `outer` occurrence is the anchor target, not the `id: outer` declaration.
    let resp = send_goto_definition(&client, 2, &uri, position_of_last(source, "outer"));
    assert!(
        resp.is_none(),
        "an ancestor's id must not resolve to a (misleading) jump target: {resp:?}"
    );

    shutdown_and_exit(&client, server_thread, 3);
}
