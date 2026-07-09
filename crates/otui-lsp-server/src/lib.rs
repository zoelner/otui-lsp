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

use lang_api::{ByteSpan, LanguageService};
use otui_core::hover::{Inheritance, StyleHover, StyleHoverKind};
use otui_core::style_index::{DocId, StyleIndex};
use otui_core::OtuiService;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverContents, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
    InitializedParams, Location, MarkupContent, MarkupKind, MessageType, OneOf,
    PositionEncodingKind, SemanticTokens, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, ServerInfo, SymbolInformation, SymbolKind, TextDocumentSyncCapability,
    TextDocumentSyncKind, Url, WorkDoneProgressOptions, WorkspaceSymbolParams,
};
use tower_lsp::{Client, LanguageServer};

use crate::position::{LineIndex, PositionEncoding};

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
    /// The workspace-wide `Name < Base` style index (spec §5.2), keyed by document URL string.
    /// Kept in sync with the document lifecycle (open/change re-index, close removes) and consumed
    /// by go-to-definition (spec §5.3). Guarded independently of [`documents`](Self::documents):
    /// the two locks are never held nested in a way that could deadlock — each is taken and released
    /// cleanly around its critical section.
    style_index: RwLock<StyleIndex>,
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
            style_index: RwLock::new(StyleIndex::new()),
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

    /// Re-index `uri`'s style definitions from `text` into the workspace [`StyleIndex`].
    ///
    /// Run on open/change; extraction is pure and cheap. The index lock is taken only for the
    /// insert, never while any document lock is held (see the [`style_index`](Self::style_index)
    /// note), so the two locks cannot deadlock.
    async fn reindex_styles(&self, uri: &Url, text: &str) {
        let defs = self.service.style_defs(text);
        self.style_index
            .write()
            .await
            .set_document(DocId::from(uri.to_string()), defs);
    }
}

/// Build an LSP [`Location`] for `span` in the document identified by `doc_id`.
///
/// A style def's spans are byte offsets into **its own** document's text, so the range must be
/// mapped against that text. Returns `None` — and the caller skips the entry — when `doc_id` is not
/// a parseable URL or its document is not currently open (its span cannot be mapped to a range; the
/// index only holds open documents today, so a workspace file-scan for closed files is a later node).
/// Shared by [`resolve_base_definition`] (go-to-definition) and [`collect_workspace_symbols`]
/// (workspace symbols).
fn resolve_location(
    doc_id: &DocId,
    span: ByteSpan,
    documents: &HashMap<Url, Document>,
    encoding: PositionEncoding,
) -> Option<Location> {
    let target_uri = Url::parse(doc_id.as_str()).ok()?;
    let target_doc = documents.get(&target_uri)?;
    Some(convert::location_of(
        target_uri,
        &target_doc.text,
        span,
        encoding,
    ))
}

/// Resolve a `Name < Base` base name to its definition site(s) (spec §5.3).
///
/// Fans the name out across the whole workspace index (the namespace is global), building an LSP
/// [`Location`] per hit against **that** target document's own text. A native `UI*` base has no
/// def in the index and so resolves to `None`. Duplicate defs (legal in the engine) each become a
/// location — one hit is a `Scalar`, several are an `Array`, zero is `None`.
///
/// Kept as a free function over borrowed state so it can be unit-tested without a live `Client`.
fn resolve_base_definition(
    index: &StyleIndex,
    documents: &HashMap<Url, Document>,
    base_name: &str,
    encoding: PositionEncoding,
) -> Option<GotoDefinitionResponse> {
    let mut locations = Vec::new();
    for (doc_id, def) in index.lookup(base_name) {
        if let Some(loc) = resolve_location(doc_id, def.name_span, documents, encoding) {
            locations.push(loc);
        }
    }

    match locations.len() {
        0 => None,
        1 => Some(GotoDefinitionResponse::Scalar(
            locations.pop().expect("len 1"),
        )),
        _ => Some(GotoDefinitionResponse::Array(locations)),
    }
}

/// Collect the workspace's `Name < Base` style definitions that match `query`, as a flat
/// [`SymbolInformation`] list for `workspace/symbol` (spec §5.2).
///
/// Matching is **case-insensitive substring** over the style name — simple and predictable, and the
/// convention the client expects (it filters further as the user types). An **empty query matches
/// everything**, so the picker opens showing all styles. Each surviving def maps its [`DocId`] back
/// to a [`Url`] and builds a [`Location`](tower_lsp::lsp_types::Location) for its `name_span` against
/// **that** target document's own text (via [`convert::location_of`]), exactly as
/// [`resolve_base_definition`] does. A def whose document is not currently open is skipped — its span
/// cannot be mapped to a range (the index only holds open documents today anyway). The widget's base
/// becomes the entry's `container_name`, giving the picker useful context; native `UI*` bases are
/// never symbols of their own (they have no def, so are absent from the index) — they surface only as
/// the `container_name` of a widget that inherits them.
///
/// Duplicate style names (legal in the engine) each produce their own entry; nothing is deduped.
/// Kept as a free function over borrowed state so it can be unit-tested without a live `Client`.
#[allow(deprecated)] // `SymbolInformation.deprecated` is a mandatory-but-deprecated struct field.
fn collect_workspace_symbols(
    index: &StyleIndex,
    documents: &HashMap<Url, Document>,
    query: &str,
    encoding: PositionEncoding,
) -> Vec<SymbolInformation> {
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for (doc_id, def) in index.iter() {
        if !def.name.to_lowercase().contains(&needle) {
            continue;
        }
        // `name_span` is a byte span into the defining document's text; a def whose document is not
        // open (or whose id is not a URL) cannot be mapped to a range and is skipped.
        let Some(location) = resolve_location(doc_id, def.name_span, documents, encoding) else {
            continue;
        };
        out.push(SymbolInformation {
            name: def.name.clone(),
            kind: SymbolKind::CLASS,
            tags: None,
            deprecated: None,
            location,
            container_name: def.base.clone(),
        });
    }
    out
}

/// Format a [`StyleHover`] description from the engine into an LSP Markdown [`Hover`] (spec §5.5).
///
/// This is pure presentation: every language decision (native vs. user base, workspace resolution,
/// definition count, inheritance) was already made by [`otui_core`]'s
/// [`style_hover_at`](OtuiService::style_hover_at); here we only turn the structured facts into
/// wording and map the description's span to a range so the client underlines the hovered token.
fn render_hover(desc: &StyleHover, line_index: &LineIndex, encoding: PositionEncoding) -> Hover {
    let value = match &desc.kind {
        StyleHoverKind::NativeBase { name } => {
            format!("**`{name}`** — built-in native widget class")
        }
        StyleHoverKind::UserBase {
            name,
            def_count,
            inherits,
        } => {
            let mut value = format!("**`{name}`** — style");
            if *def_count > 1 {
                value.push_str(&format!(" ({def_count} definitions)"));
            }
            append_inherits(&mut value, inherits.as_ref());
            value
        }
        StyleHoverKind::DanglingBase { name } => {
            format!("**`{name}`** — style (not found in workspace)")
        }
        StyleHoverKind::StyleName { name, inherits } => {
            let mut value = format!("**`{name}`** — style");
            append_inherits(&mut value, inherits.as_ref());
            value
        }
    };
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(line_index.range(desc.span.start, desc.span.end, encoding)),
    }
}

/// Append an "Inherits from `Base`" line (marking a native base as `(built-in)`) when `inherits` is
/// present; a no-op otherwise.
fn append_inherits(value: &mut String, inherits: Option<&Inheritance>) {
    if let Some(inh) = inherits {
        let native = if inh.native { " (built-in)" } else { "" };
        value.push_str(&format!("\n\nInherits from `{}`{native}", inh.base));
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
                // Go-to-definition: `Name < Base` inheritance references (spec §5.3).
                definition_provider: Some(OneOf::Left(true)),
                // Workspace symbols: the global `Name < Base` style namespace (spec §5.2).
                workspace_symbol_provider: Some(OneOf::Left(true)),
                // Hover: style names and `Name < Base` bases (spec §5.5).
                hover_provider: Some(HoverProviderCapability::Simple(true)),
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

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to resolve). Cloned so the
        // documents lock is released before we take the index lock, keeping the two locks unnested.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        // Map the cursor Position to a byte offset, then classify the token under it.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let Some(base_ref) = self.service.base_reference_at(&text, offset) else {
            return Ok(None);
        };

        // Resolve against the workspace index, building each target range from its own document.
        let index = self.style_index.read().await;
        let documents = self.documents.read().await;
        Ok(resolve_base_definition(
            &index,
            &documents,
            &base_ref.name,
            encoding,
        ))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> RpcResult<Option<Vec<SymbolInformation>>> {
        let encoding = self.encoding();
        // Take both read locks (mirroring `goto_definition`'s discipline: never nest a write lock).
        let index = self.style_index.read().await;
        let documents = self.documents.read().await;
        let symbols = collect_workspace_symbols(&index, &documents, &params.query, encoding);
        // Always return a list (empty is fine and conventional); never `None` for "no matches".
        Ok(Some(symbols))
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to hover). Cloned so the
        // documents lock is released before we take the index lock, keeping the two locks unnested.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        // Map the cursor Position to a byte offset, then let the engine describe the token under it,
        // resolving against the workspace index. Only the current doc's LineIndex is needed to map
        // the description's span back to a range.
        let line_index = LineIndex::new(&text);
        let offset = line_index.offset_at(position, encoding);
        let index = self.style_index.read().await;
        let Some(desc) = self.service.style_hover_at(&text, offset, &index) else {
            return Ok(None);
        };
        Ok(Some(render_hover(&desc, &line_index, encoding)))
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
        self.reindex_styles(&uri, &doc.text).await;
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
        self.reindex_styles(&uri, &text).await;
        self.publish(uri, &text, version).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        {
            let mut docs = self.documents.write().await;
            docs.remove(&uri);
        }
        // Drop the closed document's style defs from the workspace index.
        self.style_index
            .write()
            .await
            .remove_document(&DocId::from(uri.to_string()));
        // Clear diagnostics for the closed document.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{
        ClientCapabilities, DocumentSymbolClientCapabilities, GeneralClientCapabilities, Position,
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

    /// Build a `(StyleIndex, documents)` pair from `(uri, text)` entries, indexing each document's
    /// style defs exactly the way the backend does on open/change.
    fn workspace(entries: &[(&str, &str)]) -> (StyleIndex, HashMap<Url, Document>) {
        let svc = OtuiService::new();
        let mut index = StyleIndex::new();
        let mut documents = HashMap::new();
        for (uri_str, text) in entries {
            let uri = Url::parse(uri_str).expect("valid uri");
            index.set_document(DocId::from(uri.to_string()), svc.style_defs(text));
            documents.insert(
                uri,
                Document {
                    text: (*text).to_owned(),
                    version: 1,
                },
            );
        }
        (index, documents)
    }

    #[test]
    fn base_in_one_doc_resolves_to_the_definition_span_in_another() {
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "MyPanel < UIWidget\n"),
            ("file:///use.otui", "Child < MyPanel\n"),
        ]);
        let resp = resolve_base_definition(&index, &docs, "MyPanel", PositionEncoding::Utf16)
            .expect("resolves");
        match resp {
            GotoDefinitionResponse::Scalar(loc) => {
                assert_eq!(loc.uri.as_str(), "file:///defs.otui");
                // The name span of `MyPanel` is line 0, columns 0..7 of the *defining* document.
                assert_eq!(loc.range.start, Position::new(0, 0));
                assert_eq!(loc.range.end, Position::new(0, 7));
            }
            other => panic!("expected a scalar location, got {other:?}"),
        }
    }

    #[test]
    fn base_and_reference_in_the_same_document_resolve_within_it() {
        // A base declared and referenced in the *same* open document — the self-referencing path
        // `goto_definition` hits when a file inherits from a style it also defines.
        let (index, docs) = workspace(&[("file:///self.otui", "Base < UIWidget\nChild < Base\n")]);
        let resp = resolve_base_definition(&index, &docs, "Base", PositionEncoding::Utf16)
            .expect("resolves");
        match resp {
            GotoDefinitionResponse::Scalar(loc) => {
                assert_eq!(loc.uri.as_str(), "file:///self.otui");
                // `Base`'s defining name span is line 0, columns 0..4 of the same document.
                assert_eq!(loc.range.start, Position::new(0, 0));
                assert_eq!(loc.range.end, Position::new(0, 4));
            }
            other => panic!("expected a scalar location, got {other:?}"),
        }
    }

    #[test]
    fn native_base_resolves_to_nothing() {
        // `UIWidget` is a native built-in with no defining file, so it is absent from the index and
        // resolves to `None` (the locator still returns a `BaseRef`; the index drops it).
        let (index, docs) = workspace(&[("file:///a.otui", "MyPanel < UIWidget\n")]);
        assert!(
            resolve_base_definition(&index, &docs, "UIWidget", PositionEncoding::Utf16).is_none()
        );
    }

    #[test]
    fn duplicate_definitions_resolve_to_an_array_of_all_sites() {
        // The same style name declared in two files is legal; every def surfaces as a location.
        let (index, docs) = workspace(&[
            ("file:///a.otui", "Dup < UIWidget\n"),
            ("file:///b.otui", "Dup < UIWindow\n"),
        ]);
        let resp = resolve_base_definition(&index, &docs, "Dup", PositionEncoding::Utf16)
            .expect("resolves");
        match resp {
            GotoDefinitionResponse::Array(locs) => assert_eq!(locs.len(), 2),
            other => panic!("expected an array of locations, got {other:?}"),
        }
    }

    #[test]
    fn definition_span_of_a_closed_target_is_skipped() {
        // A def whose document is not open cannot have its span mapped to a range, so it is dropped.
        let svc = OtuiService::new();
        let mut index = StyleIndex::new();
        index.set_document(
            DocId::from("file:///closed.otui".to_owned()),
            svc.style_defs("MyPanel < UIWidget\n"),
        );
        let documents = HashMap::new(); // nothing open
        assert!(
            resolve_base_definition(&index, &documents, "MyPanel", PositionEncoding::Utf16)
                .is_none()
        );
    }

    /// Names of the symbols in `syms`, sorted for order-independent assertions (the index iterates
    /// an unordered map).
    fn sorted_names(syms: &[SymbolInformation]) -> Vec<String> {
        let mut names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
        names.sort();
        names
    }

    #[test]
    fn empty_query_returns_every_style() {
        let (index, docs) = workspace(&[
            ("file:///a.otui", "Alpha < UIWidget\nBeta < UIWindow\n"),
            ("file:///b.otui", "Gamma < UIButton\n"),
        ]);
        let syms = collect_workspace_symbols(&index, &docs, "", PositionEncoding::Utf16);
        assert_eq!(sorted_names(&syms), ["Alpha", "Beta", "Gamma"]);
    }

    #[test]
    fn query_is_a_case_insensitive_substring_filter() {
        let (index, docs) = workspace(&[(
            "file:///a.otui",
            "MainWindow < UIWindow\nMiniPanel < UIWidget\nButton < UIButton\n",
        )]);
        // `win` matches `MainWindow` (substring, case-insensitive) but not `MiniPanel`/`Button`.
        let syms = collect_workspace_symbols(&index, &docs, "win", PositionEncoding::Utf16);
        assert_eq!(sorted_names(&syms), ["MainWindow"]);
        // Uppercased query still matches.
        let syms = collect_workspace_symbols(&index, &docs, "PANEL", PositionEncoding::Utf16);
        assert_eq!(sorted_names(&syms), ["MiniPanel"]);
        // A substring in the middle matches too.
        let syms = collect_workspace_symbols(&index, &docs, "ni", PositionEncoding::Utf16);
        assert_eq!(sorted_names(&syms), ["MiniPanel"]);
        // No match → an empty list (never `None` from the collector).
        let syms = collect_workspace_symbols(&index, &docs, "zzz", PositionEncoding::Utf16);
        assert!(syms.is_empty());
    }

    #[test]
    #[allow(deprecated)] // constructing/reading `SymbolInformation` fields in assertions
    fn symbol_carries_class_kind_base_container_and_name_span_location() {
        let (index, docs) = workspace(&[("file:///defs.otui", "MyPanel < UIWidget\n")]);
        let syms = collect_workspace_symbols(&index, &docs, "MyPanel", PositionEncoding::Utf16);
        assert_eq!(syms.len(), 1);
        let sym = &syms[0];
        assert_eq!(sym.name, "MyPanel");
        // A style is a named widget type → CLASS.
        assert_eq!(sym.kind, SymbolKind::CLASS);
        // The base is surfaced as the container for context in the picker.
        assert_eq!(sym.container_name.as_deref(), Some("UIWidget"));
        // The location points at the *name span* in the defining document.
        assert_eq!(sym.location.uri.as_str(), "file:///defs.otui");
        assert_eq!(sym.location.range.start, Position::new(0, 0));
        assert_eq!(sym.location.range.end, Position::new(0, 7));
    }

    #[test]
    fn name_span_location_is_resolved_against_the_defining_document() {
        // The name is not at the document start: its span must map through that document's own text.
        let (index, docs) =
            workspace(&[("file:///defs.otui", "First < UIWidget\nSecond < UIWindow\n")]);
        let syms = collect_workspace_symbols(&index, &docs, "Second", PositionEncoding::Utf16);
        assert_eq!(syms.len(), 1);
        // `Second` is on line 1, columns 0..6.
        assert_eq!(syms[0].location.range.start, Position::new(1, 0));
        assert_eq!(syms[0].location.range.end, Position::new(1, 6));
    }

    #[test]
    fn duplicate_names_across_docs_each_produce_a_symbol() {
        let (index, docs) = workspace(&[
            ("file:///a.otui", "Dup < UIWidget\n"),
            ("file:///b.otui", "Dup < UIWindow\n"),
        ]);
        let syms = collect_workspace_symbols(&index, &docs, "Dup", PositionEncoding::Utf16);
        // Both declarations surface as their own entry — nothing is deduped.
        assert_eq!(syms.len(), 2);
        assert_eq!(sorted_names(&syms), ["Dup", "Dup"]);
    }

    #[test]
    fn native_base_query_returns_nothing() {
        // `UIWidget` is a native built-in with no def, so it is absent from the index and never a
        // symbol of its own — it only appears as a `container_name`.
        let (index, docs) = workspace(&[("file:///a.otui", "MyPanel < UIWidget\n")]);
        let syms = collect_workspace_symbols(&index, &docs, "UIWidget", PositionEncoding::Utf16);
        assert!(syms.is_empty());
    }

    #[test]
    fn symbol_of_a_closed_target_is_skipped() {
        // A def whose document is not open cannot have its name span mapped to a range, so it is
        // dropped (the index can outlive the document set in principle).
        let svc = OtuiService::new();
        let mut index = StyleIndex::new();
        index.set_document(
            DocId::from("file:///closed.otui".to_owned()),
            svc.style_defs("MyPanel < UIWidget\n"),
        );
        let documents = HashMap::new(); // nothing open
        let syms =
            collect_workspace_symbols(&index, &documents, "MyPanel", PositionEncoding::Utf16);
        assert!(syms.is_empty());
    }

    /// The Markdown string of a rendered hover (panics if it is not markup).
    fn hover_text(h: &Hover) -> &str {
        match &h.contents {
            HoverContents::Markup(m) => &m.value,
            other => panic!("expected markup contents, got {other:?}"),
        }
    }

    /// Describe the hover at the first occurrence of `needle` in `text` (via the engine) and format
    /// it — the same path the `hover` handler drives, minus the document store.
    fn hover_at(index: &StyleIndex, text: &str, needle: &str) -> Hover {
        let offset = text.find(needle).expect("needle present");
        let desc = OtuiService::new()
            .style_hover_at(text, offset, index)
            .expect("cursor is on a style token");
        let line_index = LineIndex::new(text);
        render_hover(&desc, &line_index, PositionEncoding::Utf16)
    }

    #[test]
    fn hover_on_a_user_base_shows_style_and_its_inheritance() {
        let (index, _) = workspace(&[
            ("file:///defs.otui", "MyPanel < UIWidget\n"),
            ("file:///use.otui", "Child < MyPanel\n"),
        ]);
        let h = hover_at(&index, "Child < MyPanel\n", "MyPanel");
        let text = hover_text(&h);
        assert!(text.contains("**`MyPanel`** — style"), "{text}");
        // The resolved def inherits from the native `UIWidget`.
        assert!(text.contains("Inherits from `UIWidget`"), "{text}");
        assert!(text.contains("(built-in)"), "{text}");
        assert!(!text.contains("not found"), "{text}");
    }

    #[test]
    fn hover_on_a_native_base_shows_built_in() {
        let (index, _) = workspace(&[("file:///a.otui", "MyPanel < UIWidget\n")]);
        let h = hover_at(&index, "MyPanel < UIWidget\n", "UIWidget");
        let text = hover_text(&h);
        assert!(
            text.contains("built-in native widget class"),
            "native base must read as built-in, got {text}"
        );
        assert!(!text.contains("not found"), "{text}");
    }

    #[test]
    fn hover_on_a_dangling_base_shows_not_found() {
        // `Missing` is a user name declared nowhere in the workspace.
        let (index, _) = workspace(&[("file:///a.otui", "Child < Missing\n")]);
        let h = hover_at(&index, "Child < Missing\n", "Missing");
        let text = hover_text(&h);
        assert!(
            text.contains("**`Missing`** — style (not found in workspace)"),
            "{text}"
        );
    }

    #[test]
    fn hover_on_a_duplicated_base_mentions_the_definition_count() {
        let (index, _) = workspace(&[
            ("file:///a.otui", "Dup < UIWidget\n"),
            ("file:///b.otui", "Dup < UIWindow\n"),
        ]);
        let h = hover_at(&index, "Child < Dup\n", "Dup");
        let text = hover_text(&h);
        assert!(text.contains("**`Dup`** — style"), "{text}");
        assert!(text.contains("(2 definitions)"), "{text}");
    }

    #[test]
    fn hover_on_the_declared_name_shows_the_style_and_its_base() {
        let (index, _) = workspace(&[("file:///a.otui", "MainWindow < UIWindow\n")]);
        let h = hover_at(&index, "MainWindow < UIWindow\n", "MainWindow");
        let text = hover_text(&h);
        assert!(text.contains("**`MainWindow`** — style"), "{text}");
        assert!(
            text.contains("Inherits from `UIWindow` (built-in)"),
            "{text}"
        );
    }

    #[test]
    fn hover_on_a_bare_header_name_shows_only_the_style() {
        // A bare top-level `container` (no `< Base`): the name branch must emit just the style line,
        // with no "Inherits from" suffix.
        let (index, _) = workspace(&[("file:///a.otui", "Standalone\n  id: x\n")]);
        let h = hover_at(&index, "Standalone\n  id: x\n", "Standalone");
        let text = hover_text(&h);
        assert_eq!(text, "**`Standalone`** — style");
        assert!(!text.contains("Inherits from"), "{text}");
    }

    #[test]
    fn hover_range_equals_the_hovered_token_span() {
        let (index, _) = workspace(&[("file:///a.otui", "MainWindow < UIWindow\n")]);
        let src = "MainWindow < UIWindow\n";

        // Cursor on the base: range is the base token.
        let base_hover = hover_at(&index, src, "UIWindow");
        assert_eq!(base_hover.range.unwrap().start, Position::new(0, 13));
        assert_eq!(base_hover.range.unwrap().end, Position::new(0, 21));

        // Cursor on the name: range is the name token.
        let name_hover = hover_at(&index, src, "MainWindow");
        assert_eq!(name_hover.range.unwrap().start, Position::new(0, 0));
        assert_eq!(name_hover.range.unwrap().end, Position::new(0, 10));
    }

    #[test]
    fn hover_on_a_non_header_offset_yields_nothing() {
        // A property value is not a header token: the engine describes nothing, so no hover.
        let (index, _) = workspace(&[("file:///a.otui", "MainWindow < UIWindow\n  id: main\n")]);
        let src = "MainWindow < UIWindow\n  id: main\n";
        let offset = src.find("main").expect("present");
        assert!(OtuiService::new()
            .style_hover_at(src, offset, &index)
            .is_none());
    }

    #[test]
    fn full_flow_from_cursor_position_to_resolved_definition() {
        // End to end over the pure pieces: cursor Position → byte offset → base locator → resolve.
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "MyPanel < UIWidget\n"),
            ("file:///use.otui", "Child < MyPanel\n"),
        ]);
        let request_text = "Child < MyPanel\n";
        // Cursor on the `M` of `MyPanel` (line 0, column 8).
        let position = Position::new(0, 8);
        let offset = LineIndex::new(request_text).offset_at(position, PositionEncoding::Utf16);
        let base_ref = OtuiService::new()
            .base_reference_at(request_text, offset)
            .expect("cursor is on the base");
        assert_eq!(base_ref.name, "MyPanel");

        let resp = resolve_base_definition(&index, &docs, &base_ref.name, PositionEncoding::Utf16)
            .expect("resolves");
        assert!(matches!(resp, GotoDefinitionResponse::Scalar(_)));
    }
}
