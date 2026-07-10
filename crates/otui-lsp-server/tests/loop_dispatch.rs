//! End-to-end transport test: drive the real handshake + dispatch loop over an in-memory
//! [`lsp_server::Connection`] (no stdio), proving `initialize → didOpen → hover → shutdown/exit`
//! works through `Backend::handle_request`/`handle_notification`.

use std::str::FromStr;
use std::thread;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    DidOpenTextDocumentParams, HoverParams, InitializeParams, InitializedParams, Position,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Uri,
    WorkDoneProgressParams,
};
use otui_lsp_server::{serve, Backend, Termination};

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

/// Drive the client half of the handshake: `initialize` request/response, then `initialized`.
fn client_handshake(client: &Connection) {
    client
        .sender
        .send(Message::Request(Request::new(
            RequestId::from(1),
            "initialize".to_owned(),
            InitializeParams::default(),
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
