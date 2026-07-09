//! The `otui-lsp` language server: a thin LSP 3.17 transport shell over [`otui_core`].
//!
//! All language semantics live in [`otui_core`] (via the [`lang_api::LanguageService`] contract);
//! this crate does only what the protocol boundary requires — capability negotiation, an
//! in-memory document store, byte-offset ↔ [position](position) conversion, and pushing
//! [diagnostics](convert) to the client.
//!
//! The [`Backend`] type implements [`tower_lsp::LanguageServer`]; the `otui-lsp` binary wires it
//! over stdio. The pure conversion/mapping logic in [`position`] and [`convert`] is unit-tested
//! without any real I/O.

pub mod convert;
pub mod position;

use std::collections::HashMap;
use std::sync::Mutex;

use lang_api::LanguageService;
use otui_core::OtuiService;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    InitializeParams, InitializeResult, InitializedParams, MessageType, PositionEncodingKind,
    ServerCapabilities, ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::{Client, LanguageServer};

use crate::position::PositionEncoding;

/// The LSP backend: holds the client handle, the language engine, the negotiated position
/// encoding, and the in-memory document store (full text per open URL).
#[derive(Debug)]
pub struct Backend {
    client: Client,
    service: OtuiService,
    /// Chosen during `initialize`; UTF-16 until then. Guarded by a std [`Mutex`] because it is
    /// only ever read/written for a fleeting moment, never across an `.await`.
    encoding: Mutex<PositionEncoding>,
    /// Open documents by URL, full text (text document sync = FULL).
    documents: RwLock<HashMap<Url, String>>,
}

impl Backend {
    /// Construct a backend bound to `client`, backed by a fresh [`OtuiService`].
    pub fn new(client: Client) -> Self {
        Self {
            client,
            service: OtuiService::new(),
            encoding: Mutex::new(PositionEncoding::Utf16),
            documents: RwLock::new(HashMap::new()),
        }
    }

    fn encoding(&self) -> PositionEncoding {
        *self.encoding.lock().expect("encoding mutex poisoned")
    }

    /// Run the engine over `text` and push the resulting diagnostics for `uri`.
    async fn publish(&self, uri: Url, text: &str, version: Option<i32>) {
        let core_diags = self.service.diagnostics(text);
        let lsp_diags = convert::all_to_lsp(text, &core_diags, self.encoding());
        self.client
            .publish_diagnostics(uri, lsp_diags, version)
            .await;
    }
}

/// Pick the position encoding to advertise: honor the client's preference for UTF-8 if it
/// offers one, else fall back to the protocol-default UTF-16.
fn negotiate_encoding(params: &InitializeParams) -> PositionEncoding {
    let offered = params
        .capabilities
        .general
        .as_ref()
        .and_then(|g| g.position_encodings.as_ref());
    if let Some(kinds) = offered {
        if kinds.contains(&PositionEncodingKind::UTF8) {
            return PositionEncoding::Utf8;
        }
    }
    PositionEncoding::Utf16
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        let encoding = negotiate_encoding(&params);
        *self.encoding.lock().expect("encoding mutex poisoned") = encoding;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(encoding.to_kind()),
                // FULL sync: the client resends the whole document on every change.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "otui-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "otui-lsp server ready")
            .await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = doc.uri;
        {
            let mut docs = self.documents.write().await;
            docs.insert(uri.clone(), doc.text.clone());
        }
        self.publish(uri, &doc.text, Some(doc.version)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last content change carries the entire new document text.
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let uri = params.text_document.uri;
        let text = change.text;
        {
            let mut docs = self.documents.write().await;
            docs.insert(uri.clone(), text.clone());
        }
        self.publish(uri, &text, Some(params.text_document.version))
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        {
            let mut docs = self.documents.write().await;
            docs.remove(&uri);
        }
        // Clear diagnostics for the closed document.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{ClientCapabilities, GeneralClientCapabilities};

    #[test]
    fn defaults_to_utf16_when_client_offers_nothing() {
        let params = InitializeParams::default();
        assert_eq!(negotiate_encoding(&params), PositionEncoding::Utf16);
    }

    #[test]
    fn selects_utf8_when_client_offers_it() {
        let params = InitializeParams {
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF8,
                        PositionEncodingKind::UTF16,
                    ]),
                    ..GeneralClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        };
        assert_eq!(negotiate_encoding(&params), PositionEncoding::Utf8);
    }

    #[test]
    fn keeps_utf16_when_client_offers_only_utf16() {
        let params = InitializeParams {
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![PositionEncodingKind::UTF16]),
                    ..GeneralClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        };
        assert_eq!(negotiate_encoding(&params), PositionEncoding::Utf16);
    }
}
