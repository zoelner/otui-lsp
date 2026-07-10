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
use otui_core::fixes::Fix;
use otui_core::hover::{Inheritance, StyleHover, StyleHoverKind};
use otui_core::style_index::{is_native_base, DocId, StyleIndex};
use otui_core::OtuiService;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::request::{
    GotoImplementationParams, GotoImplementationResponse, GotoTypeDefinitionParams,
    GotoTypeDefinitionResponse,
};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, CompletionOptions, CompletionParams,
    CompletionResponse, Diagnostic as LspDiagnostic, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    DocumentSymbolParams, DocumentSymbolResponse, FoldingRange, FoldingRangeParams,
    FoldingRangeProviderCapability, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverContents, HoverParams, HoverProviderCapability, ImplementationProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, Location, MarkupContent, MarkupKind,
    MessageType, NumberOrString, OneOf, PositionEncodingKind, PrepareRenameResponse,
    ReferenceParams, RenameOptions, RenameParams, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, SymbolInformation,
    SymbolKind, TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextEdit, TypeDefinitionProviderCapability, Url, WorkDoneProgressOptions, WorkspaceEdit,
    WorkspaceSymbolParams,
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

/// Resolve a style/type name to its declaration site(s) for `textDocument/typeDefinition` (spec §5.2).
///
/// The name — the type of the widget instance / style under the cursor (from
/// [`style_type_at`](OtuiService::style_type_at)) — is fanned out across **every** open document (the
/// style namespace is global), and each top-level declaration of it (from
/// [`style_declarations`](OtuiService::style_declarations)) becomes an LSP [`Location`] built against
/// **that** document's own text. A native `UI*` type has no user declaration in any document and so
/// resolves to `None`, exactly like a native base in go-to-definition. Duplicate declarations (legal)
/// each become a location — zero is `None`, one a `Scalar`, several an `Array`.
///
/// Kept as a free function over borrowed state so it can be unit-tested without a live `Client`
/// (mirroring [`resolve_base_definition`]).
fn resolve_type_definition(
    documents: &HashMap<Url, Document>,
    service: &OtuiService,
    name: &str,
    encoding: PositionEncoding,
) -> Option<GotoDefinitionResponse> {
    let mut locations = Vec::new();
    for (uri, doc) in documents {
        for span in service.style_declarations(&doc.text, name) {
            locations.push(convert::location_of(uri.clone(), &doc.text, span, encoding));
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

/// Collect the styles that derive from `name` for `textDocument/implementation` (spec §5.2).
///
/// Aggregates each open document's direct subtypes of `name` (from
/// [`direct_subtypes`](OtuiService::direct_subtypes) — every top-level `X < name` header) into
/// [`Location`]s built against that document's own text. The style namespace is global, so this fans
/// out across the whole workspace. Unlike typeDefinition, a native `UI*` name is *not* suppressed:
/// user styles commonly derive from a native base, and listing those derivations is exactly the point.
/// Returns an empty vector when nothing derives from `name`; the handler maps empty to `None`.
///
/// Kept as a free function over borrowed state so it can be unit-tested without a live `Client`
/// (mirroring [`collect_references`]).
fn collect_implementations(
    documents: &HashMap<Url, Document>,
    service: &OtuiService,
    name: &str,
    encoding: PositionEncoding,
) -> Vec<Location> {
    let mut out = Vec::new();
    for (uri, doc) in documents {
        for sub in service.direct_subtypes(&doc.text, name) {
            out.push(convert::location_of(
                uri.clone(),
                &doc.text,
                sub.span,
                encoding,
            ));
        }
    }
    out
}

/// What the cursor is on for a `textDocument/references` request (spec §5.4).
///
/// A [`StyleName`](Self::StyleName) is workspace-global (uses are collected across every open
/// document); an [`Id`](Self::Id) is document-local (uses live only in the current widget tree).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReferenceTarget {
    /// A style name — the cursor is on a top-level `Name < Base` declared name or base.
    StyleName(String),
    /// An `id:` value or an anchor-target id.
    Id(String),
}

/// Classify the token at byte `offset` in `text` into a [`ReferenceTarget`], or `None` when the
/// cursor is not on a style name or an id (spec §5.4).
///
/// A base reference and a declared name both resolve to a **style name**; an `id:` value and an
/// anchor-target id both resolve to an **id**. A bare top-level container tag is a widget instance,
/// not a style in the global namespace, so it is deliberately not a style-name target (only real
/// `style_header` names are — mirroring the workspace [`StyleIndex`]).
fn classify_reference_target(
    service: &OtuiService,
    text: &str,
    offset: usize,
) -> Option<ReferenceTarget> {
    classify_rename_target(service, text, offset).map(|(target, _span)| target)
}

/// Like [`classify_reference_target`], but also returns the byte span of the exact name/id token
/// under the cursor — what `textDocument/prepareRename` echoes back so the client pre-selects the
/// symbol to edit.
///
/// This is the single classifier both `references` and `rename` drive (via
/// [`classify_reference_target`] for the former), so the two features always agree on what the cursor
/// is on. The returned span is the base token (`X < Base`), the declared-name token (`Name < Base`),
/// or the id token (`id:` value / `<id>.edge` prefix) respectively.
fn classify_rename_target(
    service: &OtuiService,
    text: &str,
    offset: usize,
) -> Option<(ReferenceTarget, ByteSpan)> {
    // Cursor on a base → the referenced style name; its span is the base token.
    if let Some(base_ref) = service.base_reference_at(text, offset) {
        return Some((ReferenceTarget::StyleName(base_ref.name), base_ref.span));
    }
    // Cursor on a top-level `style_header`'s declared name → that style name. `base_span.is_some()`
    // distinguishes a real `style_header` (always has a base) from a bare `container` (base `None`),
    // which is not a global-namespace style.
    if let Some(header) = service.style_header_at(text, offset) {
        let on_name = header.name_span.start <= offset && offset < header.name_span.end;
        if header.base_span.is_some() && on_name {
            return Some((ReferenceTarget::StyleName(header.name), header.name_span));
        }
    }
    // Cursor on an `id:` value or an anchor-target id → that id; its span is the id token.
    service
        .id_at(text, offset)
        .map(|id_ref| (ReferenceTarget::Id(id_ref.id), id_ref.span))
}

/// Collect the LSP [`Location`]s answering a `textDocument/references` request for `target` (spec
/// §5.4), honoring `include_declaration`.
///
/// * A [`StyleName`](ReferenceTarget::StyleName) fans out across **every** open document (the style
///   namespace is global): each document's declarations (only when `include_declaration`) and base
///   references become locations, mapped against that document's own text. A native `UI*` base with
///   no user definition in the index is skipped — it has no declaration and listing all its uses is
///   low value; a name that *is* in the index (even a `UI*`-shaped user style) is collected normally.
/// * An [`Id`](ReferenceTarget::Id) is resolved **only in the current document** (`current_uri`): ids
///   can repeat across files/widgets, so cross-document id references are ambiguous and intentionally
///   out of scope. The declaration is included only when `include_declaration`.
///
/// Kept as a free function over borrowed state so it is unit-testable without a live `Client`
/// (mirroring [`resolve_base_definition`]).
fn collect_references(
    target: &ReferenceTarget,
    current_uri: &Url,
    documents: &HashMap<Url, Document>,
    index: &StyleIndex,
    service: &OtuiService,
    include_declaration: bool,
    encoding: PositionEncoding,
) -> Vec<Location> {
    let mut out = Vec::new();
    match target {
        ReferenceTarget::StyleName(name) => {
            // A native `UI*` base absent from the index has no user definition and no references
            // worth listing.
            if is_native_base(name) && index.lookup(name).is_empty() {
                return out;
            }
            for (uri, doc) in documents {
                let occ = service.style_name_occurrences(&doc.text, name);
                if include_declaration {
                    for span in occ.declarations {
                        out.push(convert::location_of(uri.clone(), &doc.text, span, encoding));
                    }
                }
                for span in occ.base_refs {
                    out.push(convert::location_of(uri.clone(), &doc.text, span, encoding));
                }
            }
        }
        ReferenceTarget::Id(id) => {
            let Some(doc) = documents.get(current_uri) else {
                return out;
            };
            let occ = service.id_occurrences(&doc.text, id);
            if include_declaration {
                if let Some(span) = occ.declaration {
                    out.push(convert::location_of(
                        current_uri.clone(),
                        &doc.text,
                        span,
                        encoding,
                    ));
                }
            }
            for span in occ.anchor_refs {
                out.push(convert::location_of(
                    current_uri.clone(),
                    &doc.text,
                    span,
                    encoding,
                ));
            }
        }
    }
    out
}

/// The JSON-RPC error returned when a `textDocument/rename` carries a `new_name` that is not a valid
/// OTML identifier (spec §rename). Rewriting occurrences with a name the grammar could not re-parse
/// would silently corrupt the document, so a bad rename is rejected rather than applied.
fn invalid_identifier_error(new_name: &str) -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error::invalid_params(format!(
        "`{new_name}` is not a valid OTML name: it must be non-empty, start with a letter or `_`, \
         and contain only letters, digits, `_` or `-`."
    ))
}

/// Build the [`WorkspaceEdit`] that renames `target` to `new_name` (spec §rename), or `None` when
/// there is nothing to rename.
///
/// * **Validation.** `new_name` must be a valid OTML identifier (grammar `IDENT`, via
///   [`is_valid_identifier`](otui_core::schema::is_valid_identifier)); otherwise an
///   `Err(invalid_params)` is returned — a broken name must never be written into the document.
/// * **Style name.** Workspace-global: every open document's declaration(s) **and** base references
///   are rewritten. Unlike `references`' `include_declaration`, a rename **always** rewrites the
///   definition. A native `UI*` base with no user definition in the index has no declaration to
///   rename, so it yields `Ok(None)` (mirroring [`collect_references`]).
/// * **Id.** Document-local: only the current document's id declaration + anchor references are
///   rewritten. Ids repeat across files, so a cross-document id rename is ambiguous and out of scope
///   (mirroring `references`).
///
/// Collision-checking (the new name already existing) is deliberately out of scope — this performs
/// the purely textual rewrite. Kept as a `Client`-free function over borrowed state so it is
/// unit-testable without a live `Client` (mirroring [`collect_references`]).
fn build_rename_edits(
    target: &ReferenceTarget,
    current_uri: &Url,
    documents: &HashMap<Url, Document>,
    index: &StyleIndex,
    service: &OtuiService,
    new_name: &str,
    encoding: PositionEncoding,
) -> RpcResult<Option<WorkspaceEdit>> {
    if !otui_core::schema::is_valid_identifier(new_name) {
        return Err(invalid_identifier_error(new_name));
    }

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    match target {
        ReferenceTarget::StyleName(name) => {
            // A native `UI*` base absent from the index has no user declaration to rename.
            if is_native_base(name) && index.lookup(name).is_empty() {
                return Ok(None);
            }
            for (uri, doc) in documents {
                let occ = service.style_name_occurrences(&doc.text, name);
                let line_index = LineIndex::new(&doc.text);
                // Declarations first, then base refs — a rename rewrites *both* (the definition is
                // always included).
                let edits: Vec<TextEdit> = occ
                    .declarations
                    .iter()
                    .chain(occ.base_refs.iter())
                    .map(|span| convert::text_edit_of(*span, new_name, &line_index, encoding))
                    .collect();
                if !edits.is_empty() {
                    changes.insert(uri.clone(), edits);
                }
            }
        }
        ReferenceTarget::Id(id) => {
            let Some(doc) = documents.get(current_uri) else {
                return Ok(None);
            };
            let occ = service.id_occurrences(&doc.text, id);
            let line_index = LineIndex::new(&doc.text);
            let mut edits = Vec::new();
            if let Some(span) = occ.declaration {
                edits.push(convert::text_edit_of(span, new_name, &line_index, encoding));
            }
            for span in occ.anchor_refs {
                edits.push(convert::text_edit_of(span, new_name, &line_index, encoding));
            }
            if !edits.is_empty() {
                changes.insert(current_uri.clone(), edits);
            }
        }
    }

    if changes.is_empty() {
        return Ok(None);
    }
    Ok(Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }))
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

/// Build the LSP quick-fix [`CodeAction`]s for the byte `range` in `text` (spec §7).
///
/// The engine computes the protocol-agnostic [`Fix`]es; here each becomes a `quickfix` `CodeAction`
/// whose [`WorkspaceEdit`] carries the fix's [`TextEdit`]s for `uri` (byte spans mapped through a
/// single [`LineIndex`] under `encoding`). Each action is linked to the client diagnostics it fixes
/// by matching the fix's diagnostic code against the codes carried in the request's
/// `context.diagnostics` — so the editor associates the fix with the right squiggle.
///
/// Kept as a free function over borrowed state (mirroring [`resolve_base_definition`]) so it is
/// unit-testable without a live `Client`.
fn build_code_actions(
    service: &OtuiService,
    uri: &Url,
    text: &str,
    range: ByteSpan,
    context: &[LspDiagnostic],
    encoding: PositionEncoding,
) -> Vec<CodeActionOrCommand> {
    let line_index = LineIndex::new(text);
    service
        .quick_fixes(text, range)
        .into_iter()
        .map(|fix| {
            let edits: Vec<TextEdit> = fix
                .edits
                .iter()
                .map(|(span, replacement)| {
                    convert::text_edit_of(*span, replacement, &line_index, encoding)
                })
                .collect();
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), edits);
            CodeActionOrCommand::CodeAction(CodeAction {
                title: fix.title.clone(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: matching_diagnostics(&fix, context),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }),
                command: None,
                is_preferred: None,
                disabled: None,
                data: None,
            })
        })
        .collect()
}

/// The client diagnostics from the request context that a [`Fix`] addresses — those whose LSP `code`
/// equals the fix's [`fixes_code`](Fix::fixes_code). `None` when none match (an empty `diagnostics`
/// array and an absent one both read as "unlinked" to clients, so `None` is the tidy choice).
fn matching_diagnostics(fix: &Fix, context: &[LspDiagnostic]) -> Option<Vec<LspDiagnostic>> {
    let matched: Vec<LspDiagnostic> = context
        .iter()
        .filter(|d| diagnostic_code(d) == Some(fix.fixes_code))
        .cloned()
        .collect();
    (!matched.is_empty()).then_some(matched)
}

/// The string diagnostic code of an LSP diagnostic, if it carries one as a string (the shape this
/// server always emits — see [`convert::to_lsp`]).
fn diagnostic_code(diag: &LspDiagnostic) -> Option<&str> {
    match &diag.code {
        Some(NumberOrString::String(s)) => Some(s.as_str()),
        _ => None,
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
                // Folding ranges: collapsible widget blocks, block-scalar bodies and comment runs
                // (spec §2).
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                // Go-to-definition: `Name < Base` inheritance references (spec §5.3).
                definition_provider: Some(OneOf::Left(true)),
                // Go-to-type-definition: from a widget instance / style to the style it is an
                // instance of — its declaration in the `Name < Base` graph (spec §5.2).
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                // Go-to-implementation: from a style to every style that derives from it
                // (`X < ThisStyle`) across the workspace (spec §5.2).
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                // Workspace symbols: the global `Name < Base` style namespace (spec §5.2).
                workspace_symbol_provider: Some(OneOf::Left(true)),
                // References: uses of a style name (workspace-global) or an `id:` (document-local)
                // (spec §5.4).
                references_provider: Some(OneOf::Left(true)),
                // Rename: a style name (workspace-global) or an `id:` (document-local), with
                // client-side prepare support so the editor pre-selects the token (spec §rename).
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                // Hover: style names and `Name < Base` bases (spec §5.5).
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                // Formatting: whole-document, conservative whitespace normalization (spec §8).
                document_formatting_provider: Some(OneOf::Left(true)),
                // Completion: the OTML closed sets (spec §6). `$` / `@` / `.` / `!` re-trigger
                // completion as those characters open a `$state` selector, an `@event` key, an
                // `anchors.<edge>` / `<target>.<edge>` dotted position, or a `!`-negated state in a
                // multi-state selector (`$hover !…`).
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "$".to_owned(),
                        "@".to_owned(),
                        ".".to_owned(),
                        "!".to_owned(),
                    ]),
                    ..CompletionOptions::default()
                }),
                // Code actions: quick-fixes for the parse-level diagnostics (spec §7). A plain
                // boolean provider — the fixes are computed on demand per request range.
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
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

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> RpcResult<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; an unknown document has nothing to fold.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        let folds = self.service.folding_ranges(&text);
        Ok(Some(convert::folds_to_lsp(&folds)))
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

    async fn goto_type_definition(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> RpcResult<Option<GotoTypeDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to resolve). Cloned so the
        // documents lock is released before we take it again for aggregation.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        // Classify the symbol under the cursor into the style name it is an instance of / declares.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let Some(type_ref) = self.service.style_type_at(&text, offset) else {
            return Ok(None);
        };

        // Fan out its declaration(s) across every open document (the namespace is global). A native
        // `UI*` type has no user declaration and so resolves to nothing.
        let documents = self.documents.read().await;
        Ok(resolve_type_definition(
            &documents,
            &self.service,
            &type_ref.name,
            encoding,
        ))
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> RpcResult<Option<GotoImplementationResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to resolve). Cloned so the
        // documents lock is released before we take it again for aggregation.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        // Classify the style name under the cursor (a header name/base, or a widget instance treated
        // as its type); implementation lists who derives from that name.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let Some(type_ref) = self.service.style_type_at(&text, offset) else {
            return Ok(None);
        };

        // Aggregate the derivations across every open document (the namespace is global). No user
        // derivations → `None` (mirroring go-to-definition's empty-is-None convention).
        let documents = self.documents.read().await;
        let locations =
            collect_implementations(&documents, &self.service, &type_ref.name, encoding);
        if locations.is_empty() {
            return Ok(None);
        }
        Ok(Some(GotoImplementationResponse::Array(locations)))
    }

    async fn references(&self, params: ReferenceParams) -> RpcResult<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
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

        // Map the cursor Position to a byte offset, then classify what it is on. A cursor on neither
        // a style name nor an id has no references.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let Some(target) = classify_reference_target(&self.service, &text, offset) else {
            return Ok(None);
        };

        // Aggregate: style names fan out across the workspace; ids stay in the current document.
        let index = self.style_index.read().await;
        let documents = self.documents.read().await;
        let locations = collect_references(
            &target,
            &uri,
            &documents,
            &index,
            &self.service,
            include_declaration,
            encoding,
        );
        Ok(Some(locations))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> RpcResult<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        let position = params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → not renameable). Cloned so the
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

        // Map the cursor Position to a byte offset, then classify the token under it. A cursor on
        // neither a style name nor an id is not renameable here → `None`.
        let line_index = LineIndex::new(&text);
        let offset = line_index.offset_at(position, encoding);
        let Some((target, span)) = classify_rename_target(&self.service, &text, offset) else {
            return Ok(None);
        };

        // A native `UI*` base has no user declaration to rename → not user-renameable, so report it
        // as unrenameable (`None`) rather than pre-selecting a token that a rename would refuse.
        if let ReferenceTarget::StyleName(name) = &target {
            let index = self.style_index.read().await;
            if is_native_base(name) && index.lookup(name).is_empty() {
                return Ok(None);
            }
        }

        // Echo the exact name/id token range so the client pre-selects it for editing.
        let range = line_index.range(span.start, span.end, encoding);
        Ok(Some(PrepareRenameResponse::Range(range)))
    }

    async fn rename(&self, params: RenameParams) -> RpcResult<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to rename). Cloned so the
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

        // Classify what the cursor is on; a cursor on neither a style name nor an id has nothing to
        // rename.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let Some((target, _span)) = classify_rename_target(&self.service, &text, offset) else {
            return Ok(None);
        };

        // Build the edits: style names fan out across the workspace; ids stay document-local. An
        // invalid `new_name` surfaces as a JSON-RPC error (never a broken edit).
        let index = self.style_index.read().await;
        let documents = self.documents.read().await;
        build_rename_edits(
            &target,
            &uri,
            &documents,
            &index,
            &self.service,
            &new_name,
            encoding,
        )
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

    async fn completion(&self, params: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let encoding = self.encoding();

        // Serve from the stored document text; an unknown document has nothing to complete.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        // Map the cursor Position to a byte offset, then ask the engine for the closed set that
        // applies. An empty list is a valid answer (no closed-set context here); return it as such
        // rather than `None`, which some clients treat as "retry".
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let items = convert::completions_to_lsp(&self.service.complete_at(&text, offset));
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let encoding = self.encoding();

        // Serve from the stored document text; an unknown document has nothing to fix.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        // Map the requested LSP range to a byte span, then let the engine compute the fixes that
        // overlap it. An empty list is a valid answer (nothing fixable here); return it as such.
        let line_index = LineIndex::new(&text);
        let range = ByteSpan::new(
            line_index.offset_at(params.range.start, encoding),
            line_index.offset_at(params.range.end, encoding),
        );
        let actions = build_code_actions(
            &self.service,
            &uri,
            &text,
            range,
            &params.context.diagnostics,
            encoding,
        );
        Ok(Some(actions))
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> RpcResult<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let encoding = self.encoding();

        // Serve from the stored document text; an unknown document has nothing to format.
        let Some(text) = self
            .documents
            .read()
            .await
            .get(&uri)
            .map(|doc| doc.text.clone())
        else {
            return Ok(None);
        };

        // Ask the engine to format. `None` means the document does not parse cleanly (parse error /
        // `ERROR`/`MISSING` node); per the safety gate we then return no edits. Otherwise reply with
        // a single whole-document replace of the formatted text.
        let Some(formatted) = self.service.format(&text) else {
            return Ok(None);
        };
        Ok(Some(vec![convert::full_document_edit(
            &text, formatted, encoding,
        )]))
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

    #[test]
    fn type_definition_of_an_instance_resolves_to_its_style_decl_across_docs() {
        // `Panel` is declared in one file and used as a widget instance in another; typeDefinition
        // from the instance lands on the declaration's name span.
        let (_index, docs) = workspace(&[
            ("file:///defs.otui", "Panel < UIWidget\n"),
            (
                "file:///use.otui",
                "MainWindow < UIWindow\n  Panel\n    id: p\n",
            ),
        ]);
        let svc = OtuiService::new();
        let resp = resolve_type_definition(&docs, &svc, "Panel", PositionEncoding::Utf16)
            .expect("resolves");
        match resp {
            GotoDefinitionResponse::Scalar(loc) => {
                assert_eq!(loc.uri.as_str(), "file:///defs.otui");
                assert_eq!(loc.range.start, Position::new(0, 0));
                assert_eq!(loc.range.end, Position::new(0, 5));
            }
            other => panic!("expected a scalar location, got {other:?}"),
        }
    }

    #[test]
    fn type_definition_of_a_native_type_resolves_to_nothing() {
        // `UIWidget` is native: no user declaration in any document → `None`.
        let (_index, docs) = workspace(&[("file:///a.otui", "Panel < UIWidget\n")]);
        let svc = OtuiService::new();
        assert!(
            resolve_type_definition(&docs, &svc, "UIWidget", PositionEncoding::Utf16).is_none()
        );
    }

    #[test]
    fn type_definition_with_duplicate_decls_is_an_array() {
        // The same style declared in two files: typeDefinition surfaces both declaration sites.
        let (_index, docs) = workspace(&[
            ("file:///a.otui", "Dup < UIWidget\n"),
            ("file:///b.otui", "Dup < UIWindow\n"),
        ]);
        let svc = OtuiService::new();
        let resp =
            resolve_type_definition(&docs, &svc, "Dup", PositionEncoding::Utf16).expect("resolves");
        match resp {
            GotoDefinitionResponse::Array(locs) => assert_eq!(locs.len(), 2),
            other => panic!("expected an array of locations, got {other:?}"),
        }
    }

    #[test]
    fn implementation_lists_derivations_across_two_docs() {
        // `Base` is derived from in two separate files; implementation aggregates both.
        let (_index, docs) = workspace(&[
            ("file:///base.otui", "Base < UIWidget\n"),
            ("file:///a.otui", "ChildA < Base\n"),
            ("file:///b.otui", "ChildB < Base\n"),
        ]);
        let svc = OtuiService::new();
        let locs = collect_implementations(&docs, &svc, "Base", PositionEncoding::Utf16);
        assert_eq!(
            sorted_locs(&locs),
            vec![
                (
                    "file:///a.otui".to_owned(),
                    Position::new(0, 0),
                    Position::new(0, 6)
                ),
                (
                    "file:///b.otui".to_owned(),
                    Position::new(0, 0),
                    Position::new(0, 6)
                ),
            ]
        );
    }

    #[test]
    fn implementation_of_a_leaf_style_is_empty() {
        // Nothing derives from `Leaf` → an empty list (the handler maps this to `None`).
        let (_index, docs) = workspace(&[("file:///a.otui", "Leaf < UIWidget\n")]);
        let svc = OtuiService::new();
        assert!(collect_implementations(&docs, &svc, "Leaf", PositionEncoding::Utf16).is_empty());
    }

    /// The `(uri, range)` of each location, sorted, for order-independent assertions (the document
    /// store iterates an unordered map).
    fn sorted_locs(locs: &[Location]) -> Vec<(String, Position, Position)> {
        let mut out: Vec<(String, Position, Position)> = locs
            .iter()
            .map(|l| (l.uri.to_string(), l.range.start, l.range.end))
            .collect();
        out.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then((a.1.line, a.1.character).cmp(&(b.1.line, b.1.character)))
        });
        out
    }

    #[test]
    fn references_to_a_style_name_span_the_declaration_and_every_base_across_docs() {
        // `MyPanel` is declared in one doc and used as a base in two others.
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "MyPanel < UIWidget\n"),
            ("file:///a.otui", "ChildA < MyPanel\n"),
            ("file:///b.otui", "ChildB < MyPanel\n"),
        ]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        // include_declaration: the declaration site plus both base references.
        let target = ReferenceTarget::StyleName("MyPanel".to_owned());
        let locs = collect_references(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            true,
            PositionEncoding::Utf16,
        );
        assert_eq!(
            sorted_locs(&locs),
            vec![
                (
                    "file:///a.otui".to_owned(),
                    Position::new(0, 9),
                    Position::new(0, 16)
                ),
                (
                    "file:///b.otui".to_owned(),
                    Position::new(0, 9),
                    Position::new(0, 16)
                ),
                (
                    "file:///defs.otui".to_owned(),
                    Position::new(0, 0),
                    Position::new(0, 7)
                ),
            ]
        );
    }

    #[test]
    fn references_exclude_the_declaration_when_not_requested() {
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "MyPanel < UIWidget\n"),
            ("file:///a.otui", "ChildA < MyPanel\n"),
        ]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        let target = ReferenceTarget::StyleName("MyPanel".to_owned());
        let locs = collect_references(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            false,
            PositionEncoding::Utf16,
        );
        // Only the base reference survives; the declaration in defs.otui is dropped.
        assert_eq!(
            sorted_locs(&locs),
            vec![(
                "file:///a.otui".to_owned(),
                Position::new(0, 9),
                Position::new(0, 16)
            )]
        );
    }

    #[test]
    fn references_to_a_native_base_without_a_user_def_are_empty() {
        // `UIWidget` is a native built-in with no user definition in the index → no references listed.
        let (index, docs) = workspace(&[("file:///a.otui", "MyPanel < UIWidget\n")]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        let target = ReferenceTarget::StyleName("UIWidget".to_owned());
        let locs = collect_references(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            true,
            PositionEncoding::Utf16,
        );
        assert!(locs.is_empty());
    }

    #[test]
    fn id_references_are_document_local() {
        // The current doc declares `header` and references it twice; another doc also declares
        // `header` but must not contribute (ids are per-document).
        let (index, docs) = workspace(&[
            (
                "file:///a.otui",
                "Panel\n  id: header\nOther\n  anchors.top: header.bottom\n",
            ),
            ("file:///b.otui", "Elsewhere\n  id: header\n"),
        ]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        let target = ReferenceTarget::Id("header".to_owned());
        let locs = collect_references(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            true,
            PositionEncoding::Utf16,
        );
        // Both locations are in a.otui only: the declaration and the anchor reference.
        assert!(locs.iter().all(|l| l.uri.as_str() == "file:///a.otui"));
        assert_eq!(locs.len(), 2);
    }

    #[test]
    fn classify_reference_target_distinguishes_names_and_ids() {
        let svc = OtuiService::new();
        // Cursor on a base → the style name.
        let src = "Child < MyPanel\n";
        let off = src.find("MyPanel").expect("present");
        assert_eq!(
            classify_reference_target(&svc, src, off),
            Some(ReferenceTarget::StyleName("MyPanel".to_owned()))
        );
        // Cursor on a declared name → the style name.
        let off = src.find("Child").expect("present");
        assert_eq!(
            classify_reference_target(&svc, src, off),
            Some(ReferenceTarget::StyleName("Child".to_owned()))
        );
        // Cursor on an `id:` value → the id.
        let src = "Panel\n  id: main\n";
        let off = src.find("main").expect("present");
        assert_eq!(
            classify_reference_target(&svc, src, off),
            Some(ReferenceTarget::Id("main".to_owned()))
        );
        // Cursor on nothing referenceable → None.
        let src = "Panel\n  width: 10\n";
        let off = src.find("10").expect("present");
        assert_eq!(classify_reference_target(&svc, src, off), None);
    }

    #[test]
    fn full_flow_from_cursor_to_id_references() {
        // Position → offset → classify → aggregate, the same path the `references` handler drives.
        let (index, docs) = workspace(&[(
            "file:///a.otui",
            "Panel\n  id: header\nOther\n  anchors.top: header.bottom\n",
        )]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        let text = "Panel\n  id: header\nOther\n  anchors.top: header.bottom\n";
        // Cursor on the anchor-target id `header`.
        let anchor = text.rfind("header").expect("present");
        let position = LineIndex::new(text).position(anchor, PositionEncoding::Utf16);
        let offset = LineIndex::new(text).offset_at(position, PositionEncoding::Utf16);
        let target = classify_reference_target(&svc, text, offset).expect("on an id");
        assert_eq!(target, ReferenceTarget::Id("header".to_owned()));
        let locs = collect_references(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            true,
            PositionEncoding::Utf16,
        );
        assert_eq!(locs.len(), 2);
    }

    #[test]
    fn prepare_rename_target_gives_the_token_range_for_a_style_name() {
        let svc = OtuiService::new();
        let src = "Child < MyPanel\n";
        // Cursor on the base `MyPanel`: the target is the style name and the span is that token.
        let off = src.find("MyPanel").expect("present");
        let (target, span) = classify_rename_target(&svc, src, off).expect("renameable");
        assert_eq!(target, ReferenceTarget::StyleName("MyPanel".to_owned()));
        assert_eq!(&src[span.start..span.end], "MyPanel");
        // Cursor on the declared name `Child`: the target is that style name, span the name token.
        let off = src.find("Child").expect("present");
        let (target, span) = classify_rename_target(&svc, src, off).expect("renameable");
        assert_eq!(target, ReferenceTarget::StyleName("Child".to_owned()));
        assert_eq!(&src[span.start..span.end], "Child");
    }

    #[test]
    fn prepare_rename_target_gives_the_token_range_for_an_id() {
        let svc = OtuiService::new();
        let src = "Panel\n  id: header\n";
        let off = src.find("header").expect("present");
        let (target, span) = classify_rename_target(&svc, src, off).expect("renameable");
        assert_eq!(target, ReferenceTarget::Id("header".to_owned()));
        assert_eq!(&src[span.start..span.end], "header");
    }

    #[test]
    fn prepare_rename_target_is_none_off_symbol() {
        let svc = OtuiService::new();
        // A property value is neither a style name nor an id → not renameable.
        let src = "Panel\n  width: 10\n";
        let off = src.find("10").expect("present");
        assert!(classify_rename_target(&svc, src, off).is_none());
    }

    #[test]
    fn rename_style_name_rewrites_declaration_and_every_base_across_docs() {
        // `MyPanel` is declared in one doc and used as a base in two others.
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "MyPanel < UIWidget\n"),
            ("file:///a.otui", "ChildA < MyPanel\n"),
            ("file:///b.otui", "ChildB < MyPanel\n"),
        ]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        let target = ReferenceTarget::StyleName("MyPanel".to_owned());
        let edit = build_rename_edits(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            "Renamed",
            PositionEncoding::Utf16,
        )
        .expect("valid new name")
        .expect("some edits");
        let changes = edit.changes.expect("changes keyed by URI");
        // All three docs are edited: the declaration in defs, plus a base ref in each of a/b.
        assert_eq!(changes.len(), 3);
        // The declaration is always rewritten (a rename includes the definition).
        let defs = &changes[&Url::parse("file:///defs.otui").expect("uri")];
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].new_text, "Renamed");
        assert_eq!(defs[0].range.start, Position::new(0, 0));
        assert_eq!(defs[0].range.end, Position::new(0, 7));
        // Each base reference (`ChildX < MyPanel`) is rewritten at columns 9..16.
        for name in ["file:///a.otui", "file:///b.otui"] {
            let e = &changes[&Url::parse(name).expect("uri")];
            assert_eq!(e.len(), 1);
            assert_eq!(e[0].new_text, "Renamed");
            assert_eq!(e[0].range.start, Position::new(0, 9));
            assert_eq!(e[0].range.end, Position::new(0, 16));
        }
    }

    #[test]
    fn rename_id_is_document_local() {
        // The current doc declares `header` and references it once; another doc also declares
        // `header` but must not be touched (ids are per-document).
        let (index, docs) = workspace(&[
            (
                "file:///a.otui",
                "Panel\n  id: header\nOther\n  anchors.top: header.bottom\n",
            ),
            ("file:///b.otui", "Elsewhere\n  id: header\n"),
        ]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        let target = ReferenceTarget::Id("header".to_owned());
        let edit = build_rename_edits(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            "footer",
            PositionEncoding::Utf16,
        )
        .expect("valid new name")
        .expect("some edits");
        let changes = edit.changes.expect("changes keyed by URI");
        // Only a.otui is edited; b.otui's identically-named id is left alone.
        assert_eq!(changes.len(), 1);
        let e = &changes[&uri];
        // The declaration + the single anchor reference are both rewritten.
        assert_eq!(e.len(), 2);
        assert!(e.iter().all(|t| t.new_text == "footer"));
    }

    #[test]
    fn rename_rejects_an_invalid_new_name() {
        let (index, docs) = workspace(&[("file:///a.otui", "MyPanel < UIWidget\n")]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        let target = ReferenceTarget::StyleName("MyPanel".to_owned());
        // A name containing a space is not a valid identifier → a JSON-RPC error, never an edit.
        let err = build_rename_edits(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            "bad name",
            PositionEncoding::Utf16,
        )
        .expect_err("an invalid new name is rejected");
        assert_eq!(err.code, tower_lsp::jsonrpc::ErrorCode::InvalidParams);
    }

    #[test]
    fn rename_of_a_native_base_is_refused() {
        // `UIWidget` is a native built-in with no user definition → no declaration to rename.
        let (index, docs) = workspace(&[("file:///a.otui", "MyPanel < UIWidget\n")]);
        let svc = OtuiService::new();
        let uri = Url::parse("file:///a.otui").expect("uri");
        let target = ReferenceTarget::StyleName("UIWidget".to_owned());
        let out = build_rename_edits(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            "Renamed",
            PositionEncoding::Utf16,
        )
        .expect("a valid new name is not itself an error");
        assert!(out.is_none(), "a native base has nothing to rename");
    }

    #[test]
    fn full_flow_from_cursor_to_rename_edits() {
        // Position → offset → classify → build, the same path the `rename` handler drives.
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "MyPanel < UIWidget\n"),
            ("file:///use.otui", "Child < MyPanel\n"),
        ]);
        let svc = OtuiService::new();
        let request_text = "Child < MyPanel\n";
        // Cursor on the `M` of `MyPanel` (line 0, column 8).
        let position = Position::new(0, 8);
        let offset = LineIndex::new(request_text).offset_at(position, PositionEncoding::Utf16);
        let (target, span) =
            classify_rename_target(&svc, request_text, offset).expect("renameable");
        assert_eq!(target, ReferenceTarget::StyleName("MyPanel".to_owned()));
        assert_eq!(&request_text[span.start..span.end], "MyPanel");
        let uri = Url::parse("file:///use.otui").expect("uri");
        let edit = build_rename_edits(
            &target,
            &uri,
            &docs,
            &index,
            &svc,
            "Renamed",
            PositionEncoding::Utf16,
        )
        .expect("valid new name")
        .expect("some edits");
        // Both the defining doc and the using doc are rewritten.
        assert_eq!(edit.changes.expect("changes").len(), 2);
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

    /// The [`CodeAction`] inside a [`CodeActionOrCommand`] (panics if it is a bare command).
    fn as_action(item: &CodeActionOrCommand) -> &CodeAction {
        match item {
            CodeActionOrCommand::CodeAction(a) => a,
            other => panic!("expected a CodeAction, got {other:?}"),
        }
    }

    /// The single `(Url, Vec<TextEdit>)` change set of an action's workspace edit.
    fn only_change(action: &CodeAction) -> (&Url, &Vec<TextEdit>) {
        let changes = action
            .edit
            .as_ref()
            .expect("has a workspace edit")
            .changes
            .as_ref()
            .expect("has changes");
        assert_eq!(changes.len(), 1, "one document is edited");
        changes.iter().next().expect("one entry")
    }

    #[test]
    fn code_action_offers_tabs_to_spaces_fix_with_a_workspace_edit() {
        let uri = Url::parse("file:///a.otui").expect("uri");
        let text = "Panel\n\tid: main\n";
        // A range over the tab-indented line, no client-supplied context diagnostics.
        let range = ByteSpan::new(6, 15);
        let actions = build_code_actions(
            &OtuiService::new(),
            &uri,
            text,
            range,
            &[],
            PositionEncoding::Utf16,
        );
        assert_eq!(actions.len(), 1);
        let action = as_action(&actions[0]);
        assert_eq!(action.title, "Convert tabs to spaces");
        assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
        let (edited_uri, edits) = only_change(action);
        assert_eq!(*edited_uri, uri);
        assert_eq!(edits.len(), 1);
        // The tab at line 1, column 0 becomes two spaces.
        assert_eq!(edits[0].range.start, Position::new(1, 0));
        assert_eq!(edits[0].range.end, Position::new(1, 1));
        assert_eq!(edits[0].new_text, "  ");
        // No context diagnostic was supplied to link to.
        assert!(action.diagnostics.is_none());
    }

    #[test]
    fn code_action_links_the_matching_context_diagnostic() {
        let uri = Url::parse("file:///a.otui").expect("uri");
        let text = "Panel\n  widht: 10\n";
        // The client sends back the unknown-property diagnostic it received for `widht`.
        let widht = text.find("widht").expect("present");
        let diag = LspDiagnostic {
            range: LineIndex::new(text).range(widht, widht + 5, PositionEncoding::Utf16),
            code: Some(NumberOrString::String("unknown-property".to_owned())),
            source: Some("otui".to_owned()),
            message: "unknown property".to_owned(),
            ..LspDiagnostic::default()
        };
        let range = ByteSpan::new(widht, widht + 5);
        let actions = build_code_actions(
            &OtuiService::new(),
            &uri,
            text,
            range,
            std::slice::from_ref(&diag),
            PositionEncoding::Utf16,
        );
        // The best suggestion is `width`; it must be linked to the supplied diagnostic.
        let action = as_action(&actions[0]);
        assert_eq!(action.title, "Did you mean `width`?");
        assert_eq!(
            action.diagnostics.as_deref(),
            Some(std::slice::from_ref(&diag))
        );
        let (_, edits) = only_change(action);
        assert_eq!(edits[0].new_text, "width");
    }

    #[test]
    fn code_action_returns_empty_when_nothing_in_range_is_fixable() {
        let uri = Url::parse("file:///a.otui").expect("uri");
        // A clean document: no diagnostics, so no fixes anywhere.
        let text = "MainWindow < UIWindow\n  id: main\n";
        let actions = build_code_actions(
            &OtuiService::new(),
            &uri,
            text,
            ByteSpan::new(0, text.len()),
            &[],
            PositionEncoding::Utf16,
        );
        assert!(actions.is_empty());
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
