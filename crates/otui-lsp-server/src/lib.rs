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
pub mod semantic;

use std::collections::HashMap;
use std::sync::Mutex;

use lang_api::LanguageService;
use otui_core::OtuiService;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolParams, DocumentSymbolResponse, InitializeParams, InitializeResult,
    InitializedParams, MessageType, OneOf, PositionEncodingKind, SemanticTokens,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Url, WorkDoneProgressOptions,
};
use tower_lsp::{Client, LanguageServer};

use crate::position::PositionEncoding;

/// An open document's full text plus the version it was last synced at.
#[derive(Debug, Clone)]
struct Document {
    /// The full document text, served back for pull-style requests (e.g. semantic tokens) and
    /// future features (hover, completion, …). Diagnostics are still published from the freshly
    /// received text directly.
    text: String,
    version: i32,
}

/// The LSP backend: holds the client handle, the language engine, the negotiated position
/// encoding, and the in-memory document store (full text per open URL).
#[derive(Debug)]
pub struct Backend {
    client: Client,
    service: OtuiService,
    /// Chosen during `initialize`; UTF-16 until then. Guarded by a std [`Mutex`] because it is
    /// only ever read/written for a fleeting moment, never across an `.await`.
    encoding: Mutex<PositionEncoding>,
    /// Whether the client negotiated `hierarchicalDocumentSymbolSupport` during `initialize`;
    /// decides the `textDocument/documentSymbol` response shape (nested vs. flat). Defaults to
    /// `false` (the LSP default when the capability is absent). Guarded like [`encoding`].
    hierarchical_symbols: Mutex<bool>,
    /// Open documents by URL, full text (text document sync = FULL) plus sync version.
    documents: RwLock<HashMap<Url, Document>>,
}

impl Backend {
    /// Construct a backend bound to `client`, backed by a fresh [`OtuiService`].
    pub fn new(client: Client) -> Self {
        Self {
            client,
            service: OtuiService::new(),
            encoding: Mutex::new(PositionEncoding::Utf16),
            hierarchical_symbols: Mutex::new(false),
            documents: RwLock::new(HashMap::new()),
        }
    }

    fn encoding(&self) -> PositionEncoding {
        *self.encoding.lock().expect("encoding mutex poisoned")
    }

    fn hierarchical_symbols(&self) -> bool {
        *self
            .hierarchical_symbols
            .lock()
            .expect("hierarchical_symbols mutex poisoned")
    }

    /// Run the engine over `text` and push the resulting diagnostics for `uri`, unless a newer
    /// edit has since superseded `version`.
    ///
    /// `did_open`/`did_change` can run concurrently, and diagnostics are computed here after the
    /// document lock has been released — so a slower computation for an older edit could
    /// otherwise overwrite diagnostics for a newer one. Guard against that by checking `version`
    /// against the latest version stored for `uri` right before publishing, and discarding stale
    /// results.
    async fn publish(&self, uri: Url, text: &str, version: i32) {
        let core_diags = self.service.diagnostics(text);
        let lsp_diags = convert::all_to_lsp(text, &core_diags, self.encoding());

        let latest = self.documents.read().await.get(&uri).map(|doc| doc.version);
        if !is_current_version(latest, version) {
            return;
        }

        self.client
            .publish_diagnostics(uri, lsp_diags, Some(version))
            .await;
    }
}

/// True if `version` is still the latest known version for a document (per `latest`, typically
/// read from the document store) — i.e. diagnostics computed for it are not stale.
fn is_current_version(latest: Option<i32>, version: i32) -> bool {
    latest == Some(version)
}

/// Pick the position encoding to advertise: the client lists `position_encodings` in preference
/// order, so honor the first one we support (UTF-8 or UTF-16), falling back to the
/// protocol-default UTF-16 if none offered are supported (or none are offered at all).
fn negotiate_encoding(params: &InitializeParams) -> PositionEncoding {
    let offered = params
        .capabilities
        .general
        .as_ref()
        .and_then(|g| g.position_encodings.as_ref());
    let Some(kinds) = offered else {
        return PositionEncoding::Utf16;
    };
    for kind in kinds {
        if *kind == PositionEncodingKind::UTF16 {
            return PositionEncoding::Utf16;
        }
        if *kind == PositionEncodingKind::UTF8 {
            return PositionEncoding::Utf8;
        }
    }
    PositionEncoding::Utf16
}

/// Whether the client can consume the hierarchical (nested) `documentSymbol` response. Per LSP
/// 3.17, a client signals this via `textDocument.documentSymbol.hierarchicalDocumentSymbolSupport`;
/// when the capability is absent the default is `false`, and the server must fall back to the flat
/// `SymbolInformation[]` shape.
fn client_supports_hierarchical_symbols(params: &InitializeParams) -> bool {
    params
        .capabilities
        .text_document
        .as_ref()
        .and_then(|td| td.document_symbol.as_ref())
        .and_then(|ds| ds.hierarchical_document_symbol_support)
        .unwrap_or(false)
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        let encoding = negotiate_encoding(&params);
        *self.encoding.lock().expect("encoding mutex poisoned") = encoding;
        *self
            .hierarchical_symbols
            .lock()
            .expect("hierarchical_symbols mutex poisoned") =
            client_supports_hierarchical_symbols(&params);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(encoding.to_kind()),
                // FULL sync: the client resends the whole document on every change.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                // Semantic highlighting: advertise a full-document provider with the legend whose
                // indices match the engine's `SemanticTokenKind`. No delta/range support, no
                // modifiers.
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: semantic::legend(),
                            range: Some(false),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                // Document symbols: the widget-hierarchy outline for a `.otui` document.
                document_symbol_provider: Some(OneOf::Left(true)),
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

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> RpcResult<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; nothing to highlight for an unknown document.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        let core_tokens = self.service.semantic_tokens(&text);
        let data = semantic::encode(&text, &core_tokens, self.encoding());

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> RpcResult<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; an unknown document has no outline.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        let core_syms = self.service.document_symbols(&text);
        // Honor the client's negotiated shape: hierarchical clients get the nested outline;
        // others must receive the flat `SymbolInformation[]` form (LSP 3.17).
        let response = if self.hierarchical_symbols() {
            DocumentSymbolResponse::Nested(convert::symbols_to_lsp(
                &text,
                &core_syms,
                self.encoding(),
            ))
        } else {
            DocumentSymbolResponse::Flat(convert::symbols_to_flat(
                &uri,
                &text,
                &core_syms,
                self.encoding(),
            ))
        };
        Ok(Some(response))
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = doc.uri;
        let version = doc.version;
        {
            let mut docs = self.documents.write().await;
            docs.insert(
                uri.clone(),
                Document {
                    text: doc.text.clone(),
                    version,
                },
            );
        }
        self.publish(uri, &doc.text, version).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last content change carries the entire new document text.
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let text = change.text;
        {
            let mut docs = self.documents.write().await;
            docs.insert(
                uri.clone(),
                Document {
                    text: text.clone(),
                    version,
                },
            );
        }
        self.publish(uri, &text, version).await;
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
    use tower_lsp::lsp_types::{
        ClientCapabilities, DocumentSymbolClientCapabilities, GeneralClientCapabilities,
        TextDocumentClientCapabilities,
    };

    #[test]
    fn defaults_to_utf16_when_client_offers_nothing() {
        let params = InitializeParams::default();
        assert_eq!(negotiate_encoding(&params), PositionEncoding::Utf16);
    }

    #[test]
    fn defaults_to_utf16_when_client_offers_an_empty_list() {
        let params = InitializeParams {
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![]),
                    ..GeneralClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        };
        assert_eq!(negotiate_encoding(&params), PositionEncoding::Utf16);
    }

    #[test]
    fn selects_utf8_when_it_is_first_in_preference_order() {
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
    fn selects_utf16_when_it_is_first_in_preference_order() {
        // Even though UTF-8 is offered, UTF-16 is listed first and must win: the client's order
        // is a preference order, not an unordered set.
        let params = InitializeParams {
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF16,
                        PositionEncodingKind::UTF8,
                    ]),
                    ..GeneralClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        };
        assert_eq!(negotiate_encoding(&params), PositionEncoding::Utf16);
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

    #[test]
    fn is_current_version_true_when_it_matches_the_latest() {
        assert!(is_current_version(Some(3), 3));
    }

    #[test]
    fn is_current_version_false_when_stale() {
        // Diagnostics computed for version 2 arriving after version 3 was already stored must
        // be discarded.
        assert!(!is_current_version(Some(3), 2));
    }

    #[test]
    fn is_current_version_false_when_document_is_unknown() {
        assert!(!is_current_version(None, 1));
    }

    fn params_with_hierarchical(support: Option<bool>) -> InitializeParams {
        InitializeParams {
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    document_symbol: Some(DocumentSymbolClientCapabilities {
                        hierarchical_document_symbol_support: support,
                        ..DocumentSymbolClientCapabilities::default()
                    }),
                    ..TextDocumentClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        }
    }

    #[test]
    fn hierarchical_symbols_default_false_when_client_is_silent() {
        // No textDocument capabilities at all → the LSP default (flat) applies.
        assert!(!client_supports_hierarchical_symbols(
            &InitializeParams::default()
        ));
        // documentSymbol present but the flag omitted → still the default.
        assert!(!client_supports_hierarchical_symbols(
            &params_with_hierarchical(None)
        ));
    }

    #[test]
    fn hierarchical_symbols_true_only_when_client_opts_in() {
        assert!(client_supports_hierarchical_symbols(
            &params_with_hierarchical(Some(true))
        ));
        assert!(!client_supports_hierarchical_symbols(
            &params_with_hierarchical(Some(false))
        ));
    }
}
