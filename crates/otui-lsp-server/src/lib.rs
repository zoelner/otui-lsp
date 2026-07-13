//! The `otui-lsp` language server: a thin LSP 3.17 transport shell over [`otui_core`].
//!
//! All language semantics live in [`otui_core`] (via the [`lang_api::LanguageService`] contract);
//! this crate does only what the protocol boundary requires — capability negotiation, an
//! in-memory document store, byte-offset ↔ [position](position) conversion, and pushing
//! [diagnostics](convert) to the client.
//!
//! The [`Backend`] type holds the synchronous request/notification dispatch; the `otui-lsp` binary
//! wires it over stdio using the low-level [`lsp_server`] transport (a single blocking receive
//! loop). The pure conversion/mapping logic in [`position`] and [`convert`] is unit-tested without
//! any real I/O.

// `lsp_types::Uri` (0.97, `fluent_uri`-backed) carries an internal `Cell` for lazy scheme/authority
// bookkeeping, so it counts as an interior-mutability type. Its `Hash`/`Eq` are defined purely over
// `as_str()`, though, so the cell never perturbs a key's hash — using `Uri` as a map key is sound.
// Allow the (false-positive) lint crate-wide rather than annotate every `Uri`-keyed map.
#![allow(clippy::mutable_key_type)]

pub mod convert;
pub mod position;
pub mod semantic;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crossbeam_channel::Sender;
use lang_api::{ByteSpan, LanguageService};
use lsp_server::{
    Connection, ErrorCode, ExtractError, Message, Notification, Request, RequestId, Response,
};
use lsp_types::request::{
    GotoImplementationParams, GotoImplementationResponse, GotoTypeDefinitionParams,
    GotoTypeDefinitionResponse,
};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, ColorInformation, ColorPresentation,
    ColorPresentationParams, ColorProviderCapability, CompletionOptions, CompletionParams,
    CompletionResponse, Diagnostic as LspDiagnostic, DiagnosticSeverity,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DocumentColorParams, DocumentFormattingParams, DocumentHighlight,
    DocumentHighlightKind, DocumentHighlightParams, DocumentLink, DocumentLinkOptions,
    DocumentLinkParams, DocumentOnTypeFormattingOptions, DocumentOnTypeFormattingParams,
    DocumentRangeFormattingParams, DocumentSymbolParams, DocumentSymbolResponse, FileChangeType,
    FileSystemWatcher, FoldingRange, FoldingRangeParams, FoldingRangeProviderCapability,
    GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, ImplementationProviderCapability, InitializeParams, InitializeResult,
    Location, LogMessageParams, MarkupContent, MarkupKind, MessageType, NumberOrString, OneOf,
    PositionEncodingKind, PrepareRenameResponse, PublishDiagnosticsParams, ReferenceParams,
    Registration, RegistrationParams, RenameOptions, RenameParams, SemanticTokens,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, SymbolInformation,
    SymbolKind, TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextEdit, TypeDefinitionProviderCapability, TypeHierarchyItem, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Uri, WorkDoneProgressOptions,
    WorkspaceEdit, WorkspaceSymbolParams,
};
// Code lens / inlay hint types (node `lens-hints`), appended separately to keep the merge with
// concurrent nodes touching the block above localized to this new `use`.
use lsp_types::{
    CodeLens, CodeLensOptions, CodeLensParams, Command, InlayHint, InlayHintKind, InlayHintLabel,
    InlayHintParams,
};
use otui_core::OtuiService;
use otui_core::fixes::Fix;
use otui_core::hover::{Inheritance, StyleHover, StyleHoverKind};
use otui_core::ids::{IdOrigin, visible_ids};
use otui_core::inlay::ancestor_hints;
use otui_core::lenses::style_lenses;
use otui_core::links::{
    PathRef, is_asset_sentinel_value, is_runtime_variable_path, resolve_asset_candidates,
};
use otui_core::lua_refs::{LuaRefIndex, scan_id_refs};
use otui_core::lua_ui_loads::scan_ui_loads;
use otui_core::lua_widgets::LuaWidgetIndex;
use otui_core::otmod::otmod_scripts;
use otui_core::property_hover::{PropertyHover, PropertyValueKind};
use otui_core::style_index::{DocId, StyleDef, StyleIndex, is_native_base};

use crate::position::{LineIndex, PositionEncoding};

/// An open document's full text, sync version and language.
#[derive(Debug, Clone)]
struct Document {
    /// The full document text, served back for pull-style requests (e.g. semantic tokens) and
    /// future features (hover, completion, …). Diagnostics are still published from the freshly
    /// received text directly.
    text: String,
    version: i32,
    /// This document's language, recorded from `didOpen`'s `languageId` — see
    /// [`Backend::is_otui_document`], the single guard every OTUI-only handler checks before this
    /// text is fed to the (OTUI-only) engine.
    language: Language,
}

/// A document's language, used to gate OTUI-only behavior away from a `.lua` buffer.
///
/// The server is about to be registered, by the separate VS Code extension, for `.lua` documents
/// too — so a Lua `getChildById('closeButton')` can jump to the paired `.otui`'s `id:`. Every
/// OTUI-only surface (the analyzer above all: it would otherwise choke on perfectly valid Lua and
/// shower every open Lua buffer with false `syntax-error`/`tab-indentation` diagnostics) must stay
/// a strict no-op on a Lua document — those surfaces belong to the user's `lua-language-server`,
/// and a second server answering them would produce duplicated or garbage entries beside the good
/// ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    /// An OTUI/OTML style/layout document: every existing behavior applies unchanged.
    Otui,
    /// A Lua script: every OTUI-only surface is a no-op here.
    Lua,
}

impl Language {
    /// Classify an opened document from **both** signals: it is Lua if the `didOpen` `languageId`
    /// says so **or** the URI looks like Lua.
    ///
    /// Belt and braces, deliberately. The client that will start sending `.lua` here is a *separate
    /// repository*; if the guard trusted `languageId` alone, one editor sending `"plaintext"` (or
    /// its own id) for a `.lua` file would silently switch the entire guard off — the analyzer would
    /// run on Lua and `style_defs` would feed the workspace `StyleIndex` with garbage. A guard whose
    /// correctness depends on a string chosen in another repo is not a guard. Either signal saying
    /// "Lua" is enough.
    ///
    /// The converse stays permissive: this server has never required a particular `languageId` for an
    /// OTUI buffer (a client may send `"otui"`, `"otml"`, or its own convention), so anything that is
    /// neither signal defaults to [`Language::Otui`], preserving today's behavior for every existing
    /// client.
    fn classify(uri: &Uri, language_id: &str) -> Self {
        if language_id.eq_ignore_ascii_case("lua") || Self::from_uri(uri) == Language::Lua {
            Language::Lua
        } else {
            Language::Otui
        }
    }

    /// Classify from the URI's file extension alone. Used when a request names a document this
    /// server has no recorded `languageId` for (never opened, or a handler that answers purely from
    /// `params` without looking up the [`Document`] — see [`Backend::is_otui_document`]).
    ///
    /// Case-insensitive (a `.LUA` file is still Lua), and the URI's query and fragment are stripped
    /// first: without that, `file:///m/mod.lua#frag` or a `git:` diff-view URI ends in `lua#frag` /
    /// `lua?{...}` and would be classified **OTUI** — the analyzer would then run over Lua. Every
    /// misclassification this function can make must fail *closed*, never open.
    ///
    /// This is the single source of truth for "is this URI Lua?". Any other place that needs the
    /// answer must call it rather than testing the string itself — the two rules drifted apart once
    /// already (the watched-files router matched `.lua` case-sensitively while this said `.LUA` was
    /// Lua), and a mixed-case file then took the OTUI branch straight into the workspace index.
    fn from_uri(uri: &Uri) -> Self {
        // Classify off the SAME view of the URI that the code which actually opens the file uses.
        // `read_indexed_file` resolves through `uri_to_file_path`, which percent-**decodes**; a
        // raw-string test does not. They disagreed, and the disagreement failed OPEN:
        // `file:///m/mod%2Elua` carries no literal dot, so the raw test answered OTUI — while the
        // reader decoded it, opened the real `mod.lua`, and fed it to `style_defs` and the workspace
        // `StyleIndex`. A classifier and a reader must not disagree about which file a URI names.
        if uri_to_file_path(uri).is_some_and(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("lua"))
        }) {
            return Language::Lua;
        }
        // Fallback for URIs that name no filesystem path (`untitled:` and other non-`file:` schemes):
        // the raw string, with query and fragment stripped — without that a `git:` diff-view URI ends
        // in `lua?{...}` and reads as OTUI.
        let s = uri.as_str();
        let path = s.split(['#', '?']).next().unwrap_or(s);
        let is_lua = path
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lua"));
        if is_lua {
            Language::Lua
        } else {
            Language::Otui
        }
    }
}

/// The LSP backend: holds the server→client message sender, the language engine, the negotiated
/// position encoding, and the in-memory document store (full text per open URL).
#[derive(Debug)]
pub struct Backend {
    /// The server→client channel (the write half of the [`lsp_server::Connection`]). Server-pushed
    /// messages — diagnostics, log messages, and the dynamic `client/registerCapability` requests —
    /// are sent here; the transport's writer thread serializes them onto stdout.
    sender: Sender<Message>,
    service: OtuiService,
    /// Chosen during `initialize`; UTF-16 until then. Guarded by a [`Mutex`] because it is
    /// read/written only for a fleeting moment, never held across other work.
    encoding: Mutex<PositionEncoding>,
    /// Whether the client negotiated `hierarchicalDocumentSymbolSupport` during `initialize`;
    /// decides the `textDocument/documentSymbol` response shape (nested vs. flat). Defaults to
    /// `false` (the LSP default when the capability is absent). Guarded like [`encoding`].
    hierarchical_symbols: Mutex<bool>,
    /// Whether the client advertised
    /// `textDocument.completion.completionItem.snippetSupport` during `initialize`; gates whether
    /// `textDocument/completion` responses carry a snippet body (`$0`/`$1` tab-stops) or fall back to
    /// the plain label — see [`convert::completion_item_to_lsp`]. Defaults to `false` (the LSP
    /// default when the capability is absent). Guarded like [`encoding`].
    snippet_support: Mutex<bool>,
    /// Whether the client advertised `workspace.codeLens.refreshSupport` during `initialize`;
    /// gates whether a workspace-index mutation (a watched-file change, or the initial scan's
    /// completion — see [`republish_open_documents`](Self::republish_open_documents) and
    /// `run_initialized`'s scan thread) ever sends a `workspace/codeLens/refresh` request. A client
    /// that never opted in is not asking to be told; sending it the request anyway would just be
    /// noise it has to shrug off. Defaults to `false` (the LSP default when the capability is
    /// absent). Guarded like [`encoding`].
    code_lens_refresh_support: Mutex<bool>,
    /// Whether the client advertised `workspace.inlayHint.refreshSupport` during `initialize`; the
    /// `workspace/inlayHint/refresh` analogue of [`code_lens_refresh_support`](Self::code_lens_refresh_support) —
    /// same gate, same call sites, same default.
    inlay_hint_refresh_support: Mutex<bool>,
    /// Open documents by URL, full text (text document sync = FULL) plus sync version. An open
    /// buffer is authoritative for its URI — it may carry unsaved edits — so it always wins over the
    /// on-disk copy cached in [`disk_texts`](Self::disk_texts). Wrapped in an [`Arc`] so the
    /// background workspace scan can consult it (to skip files that became open mid-scan) from a
    /// spawned task.
    documents: Arc<RwLock<HashMap<Uri, Document>>>,
    /// The workspace-wide `Name < Base` style index (spec §5.2), keyed by document URL string.
    /// Populated at startup by scanning every `.otui` file in the workspace roots and kept in sync
    /// via the document lifecycle (open/change re-index) and file watching
    /// (`did_change_watched_files`). Consumed by
    /// go-to-definition (spec §5.3) and the other cross-file features. Guarded independently of
    /// [`documents`](Self::documents): the two locks are never held nested in a way that could
    /// deadlock — each is taken and released cleanly around its critical section. [`Arc`] so the
    /// background scan can write into it from a spawned task.
    style_index: Arc<RwLock<StyleIndex>>,
    /// The workspace-wide **Lua** widget index: the custom style properties each widget adds in Lua
    /// and its `extends` parent, keyed by document URL string. Populated at startup by scanning
    /// `*.lua` under the workspace roots and kept in sync via file watching
    /// (`did_change_watched_files` for `*.lua`). Consumed by the widget-aware `unknown-property`
    /// check ([`OtuiService::diagnostics_with_widgets`]) so a Lua-added property (e.g. a `UITable`'s
    /// `column-style`) is not wrongly flagged. Unlike [`style_index`](Self::style_index) there is no
    /// open-buffer or disk-text tracking: Lua files are never opened as OTUI documents, so this index
    /// is fed only from disk. [`Arc`] so the background scan can write into it from a spawned task.
    lua_index: Arc<RwLock<LuaWidgetIndex>>,
    /// The workspace-wide index of **Lua→OTUI id cross-references** (spec §2.3:
    /// `getChildById`/`recursiveGetChildById`/`.ui.` chains), keyed by document URL string — the
    /// other half of the OTUI↔Lua bridge alongside [`lua_index`](Self::lua_index) (which indexes
    /// widget *definitions*, not id *references*). Populated at startup by scanning `*.lua` under
    /// the workspace roots, kept in sync via file watching, and — unlike `lua_index` — ALSO fed from
    /// an open `.lua` buffer's unsaved edits (see [`set_open_document`](Self::set_open_document)),
    /// so an edited-but-unsaved controller still resolves the bridge. Consumed by
    /// `textDocument/references`'s forward direction (an OTUI `id:` → its uses in the **paired**
    /// `.lua` file — see [`Backend::lua_forward_references`]; scoped by
    /// [`LuaRefIndex::document`], never the unscoped [`LuaRefIndex::lookup`], since an id repeats
    /// freely across modules). [`Arc`] so the background scan can write into it from a spawned task.
    lua_ref_index: Arc<RwLock<LuaRefIndex>>,
    /// On-disk text of every **indexed closed** `.lua` file that contributed at least one entry to
    /// [`lua_ref_index`](Self::lua_ref_index), keyed by its `file://` URL — the Lua-side counterpart
    /// of [`disk_texts`](Self::disk_texts), needed for the same reason: turning a
    /// [`LuaIdRef`](otui_core::lua_refs::LuaIdRef)'s byte span into an LSP `Location` requires the
    /// declaring file's own text to build a [`LineIndex`]. An **open** `.lua` buffer is resolved
    /// straight from [`documents`](Self::documents) instead (it may hold unsaved edits, authoritative
    /// over this cache) — see [`Backend::lua_text_for`], the single seam both directions of the
    /// bridge read a `.lua` file's text through. [`Arc`] so the background scan can populate it from
    /// a spawned task.
    lua_texts: Arc<RwLock<HashMap<Uri, String>>>,
    /// The workspace-wide **module association** index: for every module directory (one containing
    /// at least one `*.otmod`), the `.lua` ↔ `.otui` pairs discovered from that module's own
    /// `.otmod` `scripts:` list ([`otui_core::otmod::otmod_scripts`]) crossed with each named
    /// controller's `g_ui.loadUI`/`displayUI`/`importStyle` calls
    /// ([`otui_core::lua_ui_loads::scan_ui_loads`]) — the mechanism OTClient's own module loader uses
    /// to associate a controller with its UI, which the bridge's same-directory/same-stem fast path
    /// ([`paired_uri`]) misses for roughly half of the engine's real controllers (a `styles/`
    /// subdirectory, a differently-named UI file, …). Consulted by
    /// [`Backend::associated_uris`], which every direction of the bridge
    /// ([`Backend::lua_forward_references`]/[`Backend::lua_references`]) now goes through instead of
    /// `paired_uri` alone — `paired_uri`'s result, when present, is always included first; this index
    /// only *adds* coverage, never removes or reorders it.
    ///
    /// Populated at startup by scanning every module directory under the workspace roots (this is I/O
    /// — real `.otmod`/`.lua`/`.otui` files must be read from disk — so it lives here, never in
    /// `otui-core`), and kept fresh via file watching for `**/*.otmod`, `**/*.lua` and `**/*.otui`
    /// (see [`Backend::update_module_index_for`]). [`Arc`] so the background scan can write into it
    /// from a spawned task.
    module_ui_index: Arc<RwLock<ModuleUiIndex>>,
    /// On-disk text of every **indexed closed** `.otui` file, keyed by its `file://` URL. This is
    /// the content store that lets the aggregators map a closed file's byte span → LSP range without
    /// the file being open. For any URI also present in [`documents`](Self::documents) the open
    /// buffer wins (it may have unsaved edits); see [`merge_documents`]. [`Arc`] so the background
    /// scan can populate it from a spawned task.
    disk_texts: Arc<RwLock<HashMap<Uri, String>>>,
    /// The workspace root URLs captured during `initialize` (`workspace_folders`, else `root_uri`).
    /// Written once, then read by the background scan in `run_initialized` and by every handler that
    /// resolves a `/`-rooted asset path against the data root (`document_link`, `hover`'s asset
    /// preview, the missing-asset diagnostic — see [`Backend::workspace_root_paths`]). Empty when the
    /// client opened no folder — the server then falls back to open-docs-only indexing (and asset
    /// resolution / the missing-asset diagnostic conservatively produce nothing). Guarded by a
    /// [`Mutex`] because it is written once and read many times, never mutated concurrently with a
    /// read.
    roots: Mutex<Vec<Uri>>,
    /// Serializes the "check whether a URI is open, then write its index entry" critical section so
    /// an open buffer's index always wins over stale disk text. The background scan
    /// ([`run_initialized`](Self::run_initialized)) runs concurrently with the main loop, and both
    /// it and `did_open`/`did_change` do a check-then-write against [`documents`](Self::documents) +
    /// [`style_index`](Self::style_index)/[`disk_texts`](Self::disk_texts). Without a shared guard a
    /// `did_open` could land *between* the scan's open-check and its disk write and be clobbered.
    /// Both sides take this dedicated guard across their whole check-and-write, so they can never
    /// interleave — either the buffer index wins or the disk index writes, never a torn mix. It is a
    /// separate lock (not [`documents`](Self::documents)) so the data locks stay short-lived and
    /// unnested; the guard is always the outermost lock, so no opposing nesting can deadlock.
    /// [`Arc`] so the scan thread holds a clone.
    reindex_guard: Arc<Mutex<()>>,
    /// Set once the LSP lifecycle ends ([`signal_shutdown`](Self::signal_shutdown)) to tell the
    /// background scan thread to stop as soon as possible. The scan checks it between files and, if
    /// set, skips the remaining work and its completion refresh — so dropping the backend and joining
    /// the I/O threads never waits for a full scan. [`Arc`] so the scan thread holds a clone.
    shutdown: Arc<AtomicBool>,
    /// Memoized `.otpkg`-under-root presence, keyed by detected client root (see
    /// [`otpkg_present_under_cached`]) — Finding 3: without this, `missing_asset_diagnostics` would
    /// re-walk the entire client tree on every diagnostics pass over a document with a missing asset.
    /// [`Arc`] so the scan thread's completion refresh (which runs without a `&Backend`) shares the
    /// same cache as request-driven diagnostics, rather than warming a second one from scratch.
    otpkg_cache: Arc<RwLock<HashMap<PathBuf, bool>>>,
}

/// Largest `.otui` file the workspace scan / watcher will read into the index. A style file is a few
/// KiB in practice; anything past this is almost certainly not a hand-authored style sheet, so it is
/// skipped rather than pulled wholesale into memory.
const MAX_INDEXED_FILE_BYTES: u64 = 4 * 1024 * 1024;

impl Backend {
    /// Construct a backend that sends server→client messages on `sender`, negotiating position
    /// encoding, hierarchical-symbol support and workspace roots from the client's `params`.
    pub fn new(sender: Sender<Message>, params: &InitializeParams) -> Self {
        Self {
            sender,
            service: OtuiService::new(),
            encoding: Mutex::new(negotiate_encoding(params)),
            hierarchical_symbols: Mutex::new(client_supports_hierarchical_symbols(params)),
            snippet_support: Mutex::new(client_supports_snippets(params)),
            code_lens_refresh_support: Mutex::new(client_supports_code_lens_refresh(params)),
            inlay_hint_refresh_support: Mutex::new(client_supports_inlay_hint_refresh(params)),
            documents: Arc::new(RwLock::new(HashMap::new())),
            style_index: Arc::new(RwLock::new(StyleIndex::new())),
            lua_index: Arc::new(RwLock::new(LuaWidgetIndex::new())),
            lua_ref_index: Arc::new(RwLock::new(LuaRefIndex::new())),
            lua_texts: Arc::new(RwLock::new(HashMap::new())),
            module_ui_index: Arc::new(RwLock::new(ModuleUiIndex::new())),
            disk_texts: Arc::new(RwLock::new(HashMap::new())),
            roots: Mutex::new(workspace_roots(params)),
            reindex_guard: Arc::new(Mutex::new(())),
            shutdown: Arc::new(AtomicBool::new(false)),
            otpkg_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Signal the background workspace scan to stop as soon as possible (it checks between files),
    /// so shutdown does not wait for a full scan. Called once the LSP lifecycle ends — before the
    /// backend (and its `Sender`) is dropped and the I/O threads are joined.
    pub fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
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

    /// The current workspace roots as filesystem paths — the raw candidates a `/`-rooted OTUI asset
    /// path might be relative to before any client-root detection (see [`resolve_asset_candidates`],
    /// [`detect_client_roots`]).
    fn workspace_root_paths(&self) -> Vec<PathBuf> {
        self.roots
            .lock()
            .expect("roots mutex poisoned")
            .iter()
            .filter_map(uri_to_file_path)
            .collect()
    }

    /// Best-effort data roots for `document_link` and the hover sprite preview: the detected
    /// OTClient install root(s) ([`detect_client_roots`]) when one is found, else the raw workspace
    /// roots as a fallback. Unlike the missing-asset diagnostic, neither of these features makes an
    /// accusation — an absent link or a plain (imageless) hover is harmless — so they degrade to the
    /// pre-Finding-2 heuristic instead of going silent when no client root can be confirmed.
    ///
    /// [`resolve_asset_candidates`] treats a confirmed install root and a raw workspace-root fallback
    /// identically: the engine always mounts the install root itself, ahead of anything `init.lua`
    /// mounts (see [`otui_core::links::ASSET_MOUNT_DIRS`]'s doc comment), so there is no narrower
    /// probe shape to switch to for the confirmed case.
    fn asset_data_roots(&self, doc_dir: &Path) -> Vec<PathBuf> {
        let workspace_roots = self.workspace_root_paths();
        let client_roots = detect_client_roots(Some(doc_dir), &workspace_roots);
        if client_roots.is_empty() {
            workspace_roots
        } else {
            client_roots
        }
    }

    fn snippet_support(&self) -> bool {
        *self
            .snippet_support
            .lock()
            .expect("snippet_support mutex poisoned")
    }

    fn code_lens_refresh_support(&self) -> bool {
        *self
            .code_lens_refresh_support
            .lock()
            .expect("code_lens_refresh_support mutex poisoned")
    }

    fn inlay_hint_refresh_support(&self) -> bool {
        *self
            .inlay_hint_refresh_support
            .lock()
            .expect("inlay_hint_refresh_support mutex poisoned")
    }

    /// Send a `window/logMessage` notification to the client (the sync replacement for the old
    /// `Client::log_message`). Fire-and-forget: a closed channel at shutdown is ignored.
    fn log(&self, typ: MessageType, message: impl Into<String>) {
        let params = LogMessageParams {
            typ,
            message: message.into(),
        };
        let _ = self.sender.send(Message::Notification(Notification::new(
            "window/logMessage".to_owned(),
            params,
        )));
    }

    /// Push a `textDocument/publishDiagnostics` notification for `uri`. `version` is echoed so the
    /// client can drop stale results.
    fn send_diagnostics(&self, uri: Uri, diagnostics: Vec<LspDiagnostic>, version: Option<i32>) {
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        };
        let _ = self.sender.send(Message::Notification(Notification::new(
            "textDocument/publishDiagnostics".to_owned(),
            params,
        )));
    }

    /// Run the engine over `text` and push the resulting diagnostics for `uri`, unless a newer
    /// edit has since superseded `version`.
    ///
    /// The single-threaded loop processes `did_open`/`did_change` to completion one at a time, so
    /// the version check here is belt-and-braces: it still discards diagnostics computed for an
    /// older `version` than the one currently stored for `uri`.
    ///
    /// (The OTUI-only language guard.) A `.lua` document is never analyzed here: the OTUI parser
    /// would choke on perfectly valid Lua and shower the buffer with false
    /// `syntax-error`/`tab-indentation` diagnostics, and unknown-property hints belong to the
    /// user's `lua-language-server`, not us. An **empty** diagnostics array is still published —
    /// a client needs it to clear anything stale (e.g. from before the buffer's `languageId` was
    /// known to us) — it is only the analysis that is skipped.
    fn publish(&self, uri: Uri, text: &str, version: i32) {
        if !self.is_otui_document(&uri) {
            self.send_diagnostics(uri, Vec::new(), Some(version));
            return;
        }
        // Widget-aware diagnostics: the unknown-property check consults the workspace style + Lua
        // indexes so a Lua-added property is not wrongly hinted. With the indexes still empty (scan
        // not yet complete) this is identical to the catalog-only pass. The actual compute+send is
        // shared with the background scan's completion refresh (see `compute_and_send_diagnostics`).
        let encoding = self.encoding();
        let styles = self.style_index.read().expect("style_index lock poisoned");
        let lua = self.lua_index.read().expect("lua_index lock poisoned");
        // Same asset resolution as `document_link` (§ missing-asset diagnostic): the current
        // document's directory (for relative paths) and the workspace roots (for `/`-rooted ones).
        let doc_dir = uri_to_file_path(&uri).and_then(|p| p.parent().map(Path::to_path_buf));
        let workspace_roots = self.workspace_root_paths();
        compute_and_send_diagnostics(
            &self.sender,
            &self.service,
            &styles,
            &lua,
            &self.documents,
            encoding,
            uri,
            text,
            version,
            doc_dir.as_deref(),
            &workspace_roots,
            &self.otpkg_cache,
        );
    }

    /// Recompute and republish diagnostics for every currently open document.
    ///
    /// Widget-aware `unknown-property` diagnostics depend on the workspace style + Lua indexes, so
    /// when a watched file mutates either index the open buffers' diagnostics would otherwise go
    /// stale until their next edit. Called from the main-loop watched-files handler (which holds the
    /// sender) to refresh them immediately. A `(uri, text, version)` snapshot is taken so the
    /// documents lock is not held across [`publish`](Self::publish) (which re-reads it and the index
    /// locks); the version echoed is the current one, so nothing is dropped as stale.
    fn republish_open_documents(&self) {
        let open: Vec<(Uri, String, i32)> = {
            let docs = self.documents.read().expect("documents lock poisoned");
            docs.iter()
                // A Lua buffer has no OTUI diagnostics to go stale — republishing would just send it
                // another empty list. Same filter the background scan's refresh loop applies.
                .filter(|(_, doc)| doc.language == Language::Otui)
                .map(|(uri, doc)| (uri.clone(), doc.text.clone(), doc.version))
                .collect()
        };
        for (uri, text, version) in open {
            self.publish(uri, &text, version);
        }
    }

    /// Re-index `uri`'s style definitions from `text` into the workspace [`StyleIndex`].
    ///
    /// Run on open/change; extraction is pure and cheap. The index lock is taken only for the
    /// insert, never while any document lock is held (see the [`style_index`](Self::style_index)
    /// note), so the two locks cannot deadlock.
    fn reindex_styles(&self, uri: &Uri, text: &str) {
        let defs = self.service.style_defs(text);
        self.style_index
            .write()
            .expect("style_index lock poisoned")
            .set_document(DocId::from(uri.to_string()), defs);
    }

    /// Record `uri`'s open buffer (`text`/`version`/`language`) and, for an OTUI document,
    /// re-index its styles — as one atomic critical section, held under
    /// [`reindex_guard`](Self::reindex_guard).
    ///
    /// Shared by `did_open`/`did_change`. Taking the guard across BOTH the [`documents`](Self::documents)
    /// insert and the [`style_index`](Self::style_index) write closes the race with the background
    /// workspace scan: the scan holds the same guard across its open-check + disk-index write, so it
    /// can never observe "not open" and then overwrite this buffer's index entry with stale disk
    /// text. The individual data locks are still taken and released one at a time (never nested), and
    /// the guard is always the outermost lock, so the ordering stays deadlock-free.
    ///
    /// (The OTUI-only language guard.) A `.lua` document is never fed to `style_defs`: that is the
    /// same OTUI-only parser the analyzer uses, and running it over Lua source has no business
    /// writing into the workspace `StyleIndex` — even a spurious empty entry is scope creep for a
    /// document that was never meant to be indexed this way (`lua_index`, fed only from disk, is
    /// the real workspace-wide Lua index; see its docs). A `.lua` document IS, however, re-indexed
    /// into [`lua_ref_index`](Self::lua_ref_index) from this same open-buffer text — the bridge's
    /// reverse direction (Lua → OTUI) must see an unsaved edit, not just what is on disk.
    fn set_open_document(&self, uri: &Uri, text: &str, version: i32, language: Language) {
        let _guard = self.reindex_guard.lock().expect("reindex_guard poisoned");
        {
            let mut docs = self.documents.write().expect("documents lock poisoned");
            docs.insert(
                uri.clone(),
                Document {
                    text: text.to_owned(),
                    version,
                    language,
                },
            );
        }
        match language {
            Language::Otui => self.reindex_styles(uri, text),
            Language::Lua => self.reindex_lua_refs_open(uri, text),
        }
    }

    /// This document's language: the recorded `languageId` for an open buffer, else a guess from
    /// the URI's file extension (see [`Language::from_uri`]) for a document this server has no
    /// open-buffer record of.
    fn document_language(&self, uri: &Uri) -> Language {
        self.documents
            .read()
            .expect("documents lock poisoned")
            .get(uri)
            .map(|doc| doc.language)
            .unwrap_or_else(|| Language::from_uri(uri))
    }

    /// Whether `uri` is an OTUI document — the single guard every OTUI-only handler checks before
    /// touching a document. See [`Language`] for why this exists and what it defends against.
    fn is_otui_document(&self, uri: &Uri) -> bool {
        self.document_language(uri) == Language::Otui
    }

    /// The stored text for `uri`, but only for an OTUI document: `None` for an unknown document
    /// AND for a `.lua` one (the OTUI-only language guard). This is the seam nearly every
    /// OTUI-only handler fetches its document text through, so gating it here — rather than
    /// leaving each handler to remember an extra check — makes the guard structural: a new
    /// handler that fetches its text this way is safe by construction.
    fn otui_document_text(&self, uri: &Uri) -> Option<String> {
        let docs = self.documents.read().expect("documents lock poisoned");
        let doc = docs.get(uri)?;
        (doc.language == Language::Otui).then(|| doc.text.clone())
    }

    /// The unified text view every span→range aggregator resolves against: the OPEN buffers overlaid
    /// on the on-disk cache of closed files, open winning (see [`merge_documents`]).
    ///
    /// Built fresh per request — references/rename/etc. are user-initiated, not hot paths, so the
    /// clone cost is acceptable; it also lets us pass the merged map to the existing pure aggregators
    /// (`resolve_base_definition`, `collect_references`, …) unchanged. Both read locks are taken and
    /// released here (the returned map is owned), so no document/disk lock is held across the
    /// subsequent `style_index` read — preserving the unnested-lock discipline.
    fn merged_documents(&self) -> HashMap<Uri, Document> {
        let open = self.documents.read().expect("documents lock poisoned");
        let disk = self.disk_texts.read().expect("disk_texts lock poisoned");
        merge_documents(&open, &disk)
    }

    /// Index `uri` from its on-disk text (the closed-file path used by the initial scan, the file
    /// watcher, and `did_close`): parse `text`, store its style defs in the index and cache the text
    /// in [`disk_texts`](Self::disk_texts) so its spans stay resolvable while the file is closed.
    fn index_from_disk(&self, uri: &Uri, text: String) {
        let defs = self.service.style_defs(&text);
        self.style_index
            .write()
            .expect("style_index lock poisoned")
            .set_document(DocId::from(uri.to_string()), defs);
        self.disk_texts
            .write()
            .expect("disk_texts lock poisoned")
            .insert(uri.clone(), text);
    }

    /// Whether `uri` is currently an open buffer.
    fn is_open(&self, uri: &Uri) -> bool {
        self.documents
            .read()
            .expect("documents lock poisoned")
            .contains_key(uri)
    }

    /// Drop `uri` from both the style index and the disk-text cache (a deleted / vanished file).
    fn deindex(&self, uri: &Uri) {
        self.style_index
            .write()
            .expect("style_index lock poisoned")
            .remove_document(&DocId::from(uri.to_string()));
        self.disk_texts
            .write()
            .expect("disk_texts lock poisoned")
            .remove(uri);
    }

    /// Re-index a `*.lua` file's widget definitions into the [`lua_index`](Self::lua_index) from its
    /// on-disk `text` (the initial scan and the file watcher). Extraction is pure; there is no
    /// disk-text cache or open-buffer check because Lua is never an open OTUI document.
    fn index_lua_from_disk(&self, uri: &Uri, text: &str) {
        let defs = self.service.lua_widgets(text);
        self.lua_index
            .write()
            .expect("lua_index lock poisoned")
            .set_document(DocId::from(uri.to_string()), defs);
    }

    /// Drop `uri`'s widget definitions from the [`lua_index`](Self::lua_index) (a deleted Lua file).
    fn deindex_lua(&self, uri: &Uri) {
        self.lua_index
            .write()
            .expect("lua_index lock poisoned")
            .remove_document(&DocId::from(uri.to_string()));
    }

    /// Re-index a `*.lua` file's id cross-references into [`lua_ref_index`](Self::lua_ref_index) from
    /// its on-disk `text`, caching that text in [`lua_texts`](Self::lua_texts) so a closed file's ref
    /// spans stay resolvable into an LSP `Location`. Shared by the initial scan, the file watcher, and
    /// `did_close`'s disk re-sync (mirroring [`index_from_disk`](Self::index_from_disk) for the OTUI
    /// side of the bridge).
    fn index_lua_refs_from_disk(&self, uri: &Uri, text: String) {
        let refs = scan_id_refs(&text);
        self.lua_ref_index
            .write()
            .expect("lua_ref_index lock poisoned")
            .set_document(DocId::from(uri.to_string()), refs);
        self.lua_texts
            .write()
            .expect("lua_texts lock poisoned")
            .insert(uri.clone(), text);
    }

    /// Re-index a `*.lua` OPEN buffer's id cross-references from its (possibly unsaved) `text`. Only
    /// [`lua_ref_index`](Self::lua_ref_index) is touched here — the buffer's text itself is already
    /// authoritative in [`documents`](Self::documents), which [`lua_text_for`](Self::lua_text_for)
    /// consults first, so [`lua_texts`](Self::lua_texts) (the on-disk cache) is left alone.
    fn reindex_lua_refs_open(&self, uri: &Uri, text: &str) {
        let refs = scan_id_refs(text);
        self.lua_ref_index
            .write()
            .expect("lua_ref_index lock poisoned")
            .set_document(DocId::from(uri.to_string()), refs);
    }

    /// Drop `uri`'s cross-references from both [`lua_ref_index`](Self::lua_ref_index) and the
    /// [`lua_texts`](Self::lua_texts) disk-text cache (a deleted Lua file).
    fn deindex_lua_refs(&self, uri: &Uri) {
        self.lua_ref_index
            .write()
            .expect("lua_ref_index lock poisoned")
            .remove_document(&DocId::from(uri.to_string()));
        self.lua_texts
            .write()
            .expect("lua_texts lock poisoned")
            .remove(uri);
    }

    /// The text to resolve a `.lua` document's byte spans against, for either direction of the
    /// OTUI↔Lua bridge: the OPEN buffer when `uri` is currently open (it may hold unsaved edits, and
    /// is kept authoritative the same way [`merge_documents`] treats an open `.otui` buffer), else the
    /// on-disk cache in [`lua_texts`](Self::lua_texts). `None` when neither has it — an unindexed,
    /// unopened `.lua` file the bridge cannot resolve into.
    fn lua_text_for(&self, uri: &Uri) -> Option<String> {
        if let Some(doc) = self
            .documents
            .read()
            .expect("documents lock poisoned")
            .get(uri)
            && doc.language == Language::Lua
        {
            return Some(doc.text.clone());
        }
        self.lua_texts
            .read()
            .expect("lua_texts lock poisoned")
            .get(uri)
            .cloned()
    }

    /// Every `.otui`/`.lua` file paired with `uri` — the OTUI↔Lua bridge's full pairing rule,
    /// combining [`paired_uri`]'s same-directory/same-stem fast path with
    /// [`module_ui_index`](Self::module_ui_index)'s `.otmod`-derived association.
    ///
    /// The fast-path result, when it exists, is always first and always present — the association
    /// index only *adds* candidates, it never removes or displaces the fast path (spec §2.3's rule
    /// is correct wherever it applies; this just widens what "applies" covers). Deduplicated by URL,
    /// so a controller that happens to satisfy both is not listed twice.
    fn associated_uris(&self, uri: &Uri, target_ext: &str) -> Vec<Uri> {
        let mut out = Vec::new();
        if let Some(fast) = paired_uri(uri, target_ext) {
            out.push(fast);
        }
        let index = self
            .module_ui_index
            .read()
            .expect("module_ui_index lock poisoned");
        let extra: &[Uri] = match target_ext {
            "lua" => index.lua_files_for(uri),
            "otui" => index.otui_files_for(uri),
            _ => &[],
        };
        for u in extra {
            if !out.contains(u) {
                out.push(u.clone());
            }
        }
        out
    }

    /// Recompute exactly one module's contribution to [`module_ui_index`](Self::module_ui_index) in
    /// response to a watched-file change to `uri`, or do nothing when `uri` is not relevant
    /// (anything other than a `.otmod`, `.lua`, or `.otui` file).
    ///
    /// Runs for **every** relevant watched change, independent of the open-buffer/style-index/
    /// Lua-index handling in [`did_change_watched_files`](Self::did_change_watched_files) —
    /// including a currently-open `.otui`/`.lua` file: this index is pure disk-existence bookkeeping
    /// (a `loadUI` target either exists on disk or it does not), which does not depend on whether an
    /// editor happens to have the file open the way [`style_index`](Self::style_index)/
    /// [`disk_texts`](Self::disk_texts) do.
    ///
    /// * A changed `.otmod` always owns *its own* directory as the module directory — regardless of
    ///   whether the file still exists after the change (a DELETE): [`scan_module_dir`] re-reads that
    ///   directory, finds no `.otmod` there anymore, and returns an empty pair list, which
    ///   [`ModuleUiIndex::set_module`] treats as "remove this module" — so a deleted `.otmod`
    ///   correctly clears its module's associations rather than leaking stale ones forever.
    /// * A changed `.lua`/`.otui` file walks upward for the nearest module directory that owns it
    ///   ([`find_owning_module_dir`]) and rebuilds that module instead — covering both "a controller
    ///   gained/lost a `loadUI` call" and "a `loadUI` target was created/deleted" with the same
    ///   rebuild, since [`scan_module_dir`] re-derives the whole association from disk every time.
    fn update_module_index_for(&self, uri: &Uri) {
        let Some(path) = uri_to_file_path(uri) else {
            return;
        };
        let is_otmod = is_otmod_uri(uri);
        let is_lua = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("lua"));
        let is_otui = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("otui"));
        if !(is_otmod || is_lua || is_otui) {
            return;
        }
        let module_dir = if is_otmod {
            path.parent().map(Path::to_path_buf)
        } else {
            let roots = self.workspace_root_paths();
            path.parent()
                .and_then(|start| find_owning_module_dir(start, &roots))
        };
        let Some(module_dir) = module_dir else {
            return;
        };
        let workspace_roots = self.workspace_root_paths();
        let client_roots = detect_client_roots(Some(&module_dir), &workspace_roots);
        let pairs = scan_module_dir(&module_dir, &client_roots);
        self.module_ui_index
            .write()
            .expect("module_ui_index lock poisoned")
            .set_module(module_dir, pairs);
    }
}

/// Compute widget-aware diagnostics for one document and publish them, unless a newer version has
/// superseded `version`. Shared by the request-driven [`Backend::publish`] and the background scan's
/// completion refresh (which runs without a `&Backend`), so every dependency is passed explicitly.
///
/// The style + Lua index read locks are held by the caller across this call (the analysis borrows
/// them); the documents lock is then taken only to read the current version. This ordering cannot
/// deadlock: no path holds the documents *write* lock while waiting on an index lock — the index
/// writers ([`Backend::set_open_document`], the scan) take the documents lock, if at all, in a
/// separate released scope first.
#[allow(clippy::too_many_arguments)]
fn compute_and_send_diagnostics(
    sender: &Sender<Message>,
    service: &OtuiService,
    styles: &StyleIndex,
    lua: &LuaWidgetIndex,
    documents: &RwLock<HashMap<Uri, Document>>,
    encoding: PositionEncoding,
    uri: Uri,
    text: &str,
    version: i32,
    doc_dir: Option<&Path>,
    workspace_roots: &[PathBuf],
    otpkg_cache: &RwLock<HashMap<PathBuf, bool>>,
) {
    // One parse serves both passes (see `OtuiService::diagnostics_with_widgets_and_links`'s doc
    // comment): computing the widget diagnostics and the asset-path links separately would parse
    // `text` twice for this single request, on every keystroke.
    let (core_diags, asset_links) = service.diagnostics_with_widgets_and_links(text, styles, lua);
    let mut lsp_diags = convert::all_to_lsp(text, &core_diags, encoding);
    lsp_diags.extend(missing_asset_diagnostics(
        asset_links,
        text,
        doc_dir,
        workspace_roots,
        otpkg_cache,
        encoding,
    ));
    let latest = documents
        .read()
        .expect("documents lock poisoned")
        .get(&uri)
        .map(|doc| doc.version);
    if !is_current_version(latest, version) {
        return;
    }
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics: lsp_diags,
        version: Some(version),
    };
    let _ = sender.send(Message::Notification(Notification::new(
        "textDocument/publishDiagnostics".to_owned(),
        params,
    )));
}

/// Ask the client to re-request its currently-shown code lenses and/or inlay hints — fire-and-forget
/// server→client requests, like [`Backend::register_capability`] (the client's ack, if any, arrives
/// as a `Message::Response` the main loop ignores; we do not track it). Each of the two requests is
/// gated independently on whether the client advertised refresh support for it
/// ([`client_supports_code_lens_refresh`] / [`client_supports_inlay_hint_refresh`]).
///
/// A free function, not a `Backend` method, for the same reason [`compute_and_send_diagnostics`] is:
/// the background workspace-scan thread only holds cloned pieces of `Backend` (never `&Backend`), so
/// every dependency — here, just the sender and the two booleans, copied out before the thread spawns
/// — is passed explicitly.
///
/// Called wherever a workspace index mutation (a watched `.otui`/`.lua` file change, or the initial
/// scan's completion) could have made an already-computed code lens ("N widgets inherit this style")
/// or inlay hint (a cross-file-resolved native ancestor) in ANOTHER open document stale. The lens/hint
/// for the document that was itself just edited is not this function's concern — a client re-invokes
/// `textDocument/codeLens`/`textDocument/inlayHint` for a buffer on every edit to it regardless, since
/// its content (and so its `Range`s) changed; it is every *other* open document, whose text never
/// changed but whose already-computed lenses/hints now read stale data, that has no other trigger to
/// re-ask.
fn send_workspace_refresh(
    sender: &Sender<Message>,
    code_lens_refresh_support: bool,
    inlay_hint_refresh_support: bool,
) {
    if code_lens_refresh_support {
        let _ = sender.send(Message::Request(Request::new(
            RequestId::from("otui-code-lens-refresh".to_owned()),
            "workspace/codeLens/refresh".to_owned(),
            (),
        )));
    }
    if inlay_hint_refresh_support {
        let _ = sender.send(Message::Request(Request::new(
            RequestId::from("otui-inlay-hint-refresh".to_owned()),
            "workspace/inlayHint/refresh".to_owned(),
            (),
        )));
    }
}

/// Merge the open buffers over the on-disk cache into a single `URI → Document` view for one
/// request.
///
/// The on-disk cache seeds the map (every indexed closed file), then each open buffer is inserted on
/// top — so **an open buffer always wins over the on-disk copy for the same URI** (it may hold
/// unsaved edits that are authoritative over what is on disk). Closed files carry `version` 0, which
/// is irrelevant here: the aggregators only read `.text`.
///
/// Pure over borrowed state so the open-vs-disk precedence is unit-testable without any real I/O.
fn merge_documents(
    open: &HashMap<Uri, Document>,
    disk: &HashMap<Uri, String>,
) -> HashMap<Uri, Document> {
    // Every entry in the result must be OTUI, and that is load-bearing: the aggregators that consume
    // this map (`collect_references`, `build_rename_edits`) run the **OTUI parser** over every
    // document in it, and the OTUI grammar does not reject Lua — a column-0 line like `x < limit)`
    // (the tail of a wrapped expression) parses as a `style_header` named `x`. A Lua document reaching
    // here therefore means `textDocument/rename` of an OTUI style `x` emits a `TextEdit` **rewriting
    // bytes inside that file**.
    //
    // BOTH halves are filtered, not just the open one. The disk half is nominally all-`.otui` — but
    // that invariant is upheld by string tests in other functions, and one of them (the watched-files
    // router) was case-sensitive while the language classifier was not, so a `Mod.LUA` walked straight
    // into `disk_texts`. A seam that trusts an invariant enforced somewhere else is not a seam. This
    // one re-checks, so it holds even when an upstream writer is wrong.
    let mut merged: HashMap<Uri, Document> = disk
        .iter()
        .filter(|(uri, _)| Language::from_uri(uri) == Language::Otui)
        .map(|(uri, text)| {
            (
                uri.clone(),
                Document {
                    text: text.clone(),
                    version: 0,
                    language: Language::Otui,
                },
            )
        })
        .collect();
    // Open buffers override any stale on-disk entry for the same URI — but only OTUI ones. This is
    // the language guard at the seam: filtering here keeps every consumer correct at once, instead of
    // asking each future aggregator to remember a check it cannot be reminded of.
    for (uri, doc) in open {
        if doc.language != Language::Otui {
            continue;
        }
        merged.insert(uri.clone(), doc.clone());
    }
    merged
}

/// Read a `file://` `.otui` document from disk for indexing, or `None` when it cannot / should not be
/// indexed.
///
/// Returns `None` — and the caller skips it — for a non-`file:` URI, an unreadable path, a file
/// larger than [`MAX_INDEXED_FILE_BYTES`], or content that is not valid UTF-8 (a binary/garbage file
/// must never crash the server or land bogus entries in the index). This is the single disk-read seam
/// shared by the scan, the watcher and `did_close`.
/// Convert a `file:` [`Uri`] to a filesystem path, or `None` for a non-`file:` URI or one that
/// does not map to a valid path. `lsp_types::Uri` (0.97, `fluent_uri`-backed) carries no
/// file-path helpers, so the well-tested `url` crate does the percent-decoding and platform path
/// mapping — the exact behaviour the server relied on before the 0.97 bump.
fn uri_to_file_path(uri: &Uri) -> Option<PathBuf> {
    url::Url::parse(uri.as_str()).ok()?.to_file_path().ok()
}

/// Build a `file:` [`Uri`] from a filesystem path, or `None` if the path cannot be represented as
/// a `file:` URL. Mirror of [`uri_to_file_path`]; see it for why the `url` crate is used.
fn uri_from_file_path(path: &Path) -> Option<Uri> {
    Uri::from_str(url::Url::from_file_path(path).ok()?.as_str()).ok()
}

/// The sibling `.otui`/`.lua` file paired with `uri` — same directory, same file stem, differing
/// only in extension (spec §2.3: an OTUI module and its controller are paired "the same way OTClient
/// pairs them: same directory, same name" — e.g. `login.otui` pairs with `login.lua`). This is the
/// correctness boundary for the OTUI↔Lua bridge (both directions): an id repeats freely across the
/// workspace, so `textDocument/references` must resolve against exactly this one sibling file, never
/// fan out to every `.lua`/`.otui` that happens to mention the same id (see
/// [`Backend::lua_forward_references`] and [`Backend::lua_references`]).
///
/// Goes through [`uri_to_file_path`] (which percent-*decodes*) rather than a raw string swap, so a
/// URI carrying a space or an encoded character (`%20`, `foo%2Elua`) pairs the same file the reader
/// that indexes it (`read_indexed_file`) would open — the two must never disagree about which file a
/// URI names (see [`Language::from_uri`]'s doc comment for the class of bug that causes). `None` for
/// a non-`file:` URI or one with no representable sibling path.
///
/// **Coverage.** This is exactly the pairing rule spec §2.3 specifies, and it is correct wherever a
/// sibling exists — but not every controller has one. Measured on the real engine corpus (`otclient`):
/// of the `.lua` files that call `getChildById`/`recursiveGetChildById` at all, only *roughly half*
/// have a same-directory/same-stem `.otui` to pair with here. The rest keep their UI elsewhere — a
/// `styles/` subdirectory (e.g. `game_rewardwall/game_rewardwall.lua` ↔
/// `game_rewardwall/styles/style.otui`) or an altogether different filename/module layout (e.g.
/// `game_wheel/classes/*`) — and this function alone finds nothing for them: it does not, and cannot
/// (it has no notion of a "module"), discover a controller/UI pair that does not share a directory and
/// stem. [`Backend::associated_uris`] closes that gap by additionally consulting
/// [`ModuleUiIndex`] — the association OTClient's own module loader actually uses (a module's
/// `.otmod` names its controllers; each controller's `g_ui.loadUI`/`displayUI`/`importStyle` calls
/// name its UI files) — so this function's result, whenever it exists, is only ever the fast path,
/// never the whole story.
fn paired_uri(uri: &Uri, target_ext: &str) -> Option<Uri> {
    let path = uri_to_file_path(uri)?;
    uri_from_file_path(&path.with_extension(target_ext))
}

/// The workspace-wide module-association index: for every module directory found under the
/// workspace roots (one containing at least one `*.otmod`), the `.lua` ↔ `.otui` pairs that
/// directory's own module declares — see [`Backend::module_ui_index`]'s doc comment for the
/// mechanism and [`scan_module_dir`] for how one module's pairs are computed.
///
/// Keyed internally by module directory (`by_module`) rather than flattened straight into the two
/// derived lookup maps, so a later change to exactly one module ([`Backend::update_module_index_for`])
/// can replace that module's contribution ([`set_module`](Self::set_module)) without disturbing any
/// other module's pairs, then cheaply rebuild the two derived maps from scratch — cheap because a
/// real workspace has on the order of a hundred modules, not a hundred thousand.
#[derive(Debug, Default)]
struct ModuleUiIndex {
    by_module: HashMap<PathBuf, Vec<(Uri, Uri)>>,
    lua_to_otui: HashMap<Uri, Vec<Uri>>,
    otui_to_lua: HashMap<Uri, Vec<Uri>>,
}

impl ModuleUiIndex {
    fn new() -> Self {
        Self::default()
    }

    /// Replace `module_dir`'s contribution with `pairs` (each `(controller_uri, ui_uri)`), then
    /// rebuild the derived lookup maps. An empty `pairs` removes the module entirely — the shape
    /// [`Backend::update_module_index_for`] relies on to clear a module whose `.otmod` was deleted,
    /// or that no longer declares any resolvable pair.
    fn set_module(&mut self, module_dir: PathBuf, pairs: Vec<(Uri, Uri)>) {
        if pairs.is_empty() {
            self.by_module.remove(&module_dir);
        } else {
            self.by_module.insert(module_dir, pairs);
        }
        self.rebuild_derived();
    }

    fn rebuild_derived(&mut self) {
        self.lua_to_otui.clear();
        self.otui_to_lua.clear();
        for pairs in self.by_module.values() {
            for (lua, otui) in pairs {
                let lua_list = self.lua_to_otui.entry(lua.clone()).or_default();
                if !lua_list.contains(otui) {
                    lua_list.push(otui.clone());
                }
                let otui_list = self.otui_to_lua.entry(otui.clone()).or_default();
                if !otui_list.contains(lua) {
                    otui_list.push(lua.clone());
                }
            }
        }
    }

    /// Every `.otui` file `lua_uri` is associated with (its module's `.otmod` names it as a
    /// controller, and it calls `loadUI`/`displayUI`/`importStyle` on that `.otui`), or an empty
    /// slice when `lua_uri` is not indexed here at all.
    fn otui_files_for(&self, lua_uri: &Uri) -> &[Uri] {
        self.lua_to_otui.get(lua_uri).map_or(&[], Vec::as_slice)
    }

    /// The reverse of [`otui_files_for`](Self::otui_files_for): every `.lua` controller associated
    /// with `otui_uri`.
    fn lua_files_for(&self, otui_uri: &Uri) -> &[Uri] {
        self.otui_to_lua.get(otui_uri).map_or(&[], Vec::as_slice)
    }

    /// The number of module directories currently indexed (log/test visibility, mirroring
    /// [`otui_core::lua_refs::LuaRefIndex::document_count`]).
    fn module_count(&self) -> usize {
        self.by_module.len()
    }
}

/// Whether `uri` names a `.otmod` file (case-insensitive extension match, like
/// [`Language::from_uri`]'s `.lua` check) — a module manifest, never a real `.otui`/`.lua` document
/// in its own right. `did_change_watched_files` routes it away from both the style-index and
/// Lua-index branches before either can misread it (see the call site).
fn is_otmod_uri(uri: &Uri) -> bool {
    uri_to_file_path(uri).is_some_and(|p| {
        p.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("otmod"))
    })
}

/// Whether `dir` itself directly contains at least one `*.otmod` file (non-recursive — a module's
/// manifest always sits directly in its own directory in the real engine corpus).
fn dir_has_otmod(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("otmod"))
    })
}

/// Walk upward from `start` (inclusive) looking for the nearest ancestor directory that directly
/// contains a `*.otmod` — "the module `start` lives inside", if any. The walk never goes above a
/// known workspace root: a `.lua`/`.otui` file living outside any module (most of `data/`, for
/// instance) must not pay the cost of walking all the way to the filesystem root only to find
/// nothing, and must not accidentally associate with some unrelated module sitting further up an
/// unbounded tree. Mirrors [`find_client_root`]'s upward-walk shape.
fn find_owning_module_dir(start: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if dir_has_otmod(d) {
            return Some(d.to_path_buf());
        }
        if roots.iter().any(|r| r == d) {
            return None;
        }
        dir = d.parent();
    }
    None
}

/// Recursively collect every directory under `roots` that directly contains at least one `*.otmod`
/// file — the full set of module directories the initial workspace scan needs to build
/// [`ModuleUiIndex`] from scratch. Symlinks are not followed (same discipline as
/// [`collect_files_under`], for the same reason: the walk must not escape the root or loop).
fn find_module_dirs(roots: &[Uri]) -> Vec<PathBuf> {
    let mut out = HashSet::new();
    for root in roots {
        if let Some(dir) = uri_to_file_path(root) {
            collect_module_dirs(&dir, &mut out);
        }
    }
    out.into_iter().collect()
}

fn collect_module_dirs(dir: &Path, out: &mut HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut has_otmod = false;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            subdirs.push(path);
        } else if meta.is_file()
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("otmod"))
        {
            has_otmod = true;
        }
    }
    if has_otmod {
        out.insert(dir.to_path_buf());
    }
    for sub in subdirs {
        collect_module_dirs(&sub, out);
    }
}

/// Compute the full `.lua` ↔ `.otui` association for one module directory — everything
/// [`Backend::module_ui_index`]'s doc comment describes, for exactly this module:
///
/// 1. Read every `*.otmod` directly inside `module_dir` (ordinarily exactly one) and extract its
///    `scripts:` list ([`otmod_scripts`]). Each entry, resolved relative to `module_dir` with
///    `.lua` implied, is a candidate controller — **except** an entry ending in `*` (module doc
///    comment: OTClient's own directory-wildcard form), which is expanded here via
///    [`collect_files_under`] instead of treated as a single file, since resolving it needs exactly
///    the recursive-listing capability that helper already provides for the workspace-wide `.lua`
///    scan.
/// 2. Read each candidate controller that actually exists on disk (via [`read_indexed_file`], so an
///    oversized/binary/unreadable one is silently skipped, matching every other reader in this
///    file) and scan it for `g_ui.loadUI`/`displayUI`/`importStyle` calls
///    ([`scan_ui_loads`]).
/// 3. Resolve each call's argument relative to **that controller's own directory** — not
///    `module_dir` — with `.otui` implied, and keep the pair only when that target **actually
///    exists on disk** — the correctness rule this whole mechanism rests on (a load whose target
///    does not exist would be a false pairing dressed up as a real one; see the module doc
///    comment's "no false pairing" rule). The controller's own directory, mirroring the engine's
///    `LuaInterface::loadScript`, which resolves a relative argument against
///    `getCurrentSourcePath()` — the directory of whichever `.lua` source is *currently executing*
///    the call. That coincides with `module_dir` for a top-level controller, but not for a nested
///    one: `game_cyclopedia/tab/bestiary/bestiary.lua`'s `g_ui.loadUI("bestiary")` (real corpus
///    example) resolves against `tab/bestiary/`, where its sibling `bestiary.otui` actually lives —
///    resolving against `module_dir` instead would look for a `game_cyclopedia/bestiary.otui` that
///    does not exist and silently drop the pairing.
///
/// Best-effort throughout, matching the wildcard/variable-argument tolerances
/// [`otmod_scripts`]/[`scan_ui_loads`] already document: a missing `.otmod`, a controller that
/// loads nothing, or a `loadUI` argument that is a variable simply contributes no pair — never a
/// panic, never a guess. A `loadUI` argument starting with `/` is a VFS-absolute path (see
/// [`resolve_vfs_rooted_otui`]'s doc comment for the engine trace) resolved against `client_roots`
/// when at least one was detected for this module directory; with none detected, it is left
/// unresolved rather than guessed at — exactly [`detect_client_roots`]'s existing "silence, not a
/// guess" contract for a `/`-rooted path.
fn scan_module_dir(module_dir: &Path, client_roots: &[PathBuf]) -> Vec<(Uri, Uri)> {
    let mut controllers: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(module_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("otmod"))
            {
                continue;
            }
            let Some(otmod_uri) = uri_from_file_path(&path) else {
                continue;
            };
            let Some(text) = read_indexed_file(&otmod_uri) else {
                continue;
            };
            for script in otmod_scripts(&text) {
                if let Some(sub) = script.strip_suffix('*') {
                    let listing_dir = module_dir.join(sub.trim_end_matches('/'));
                    let mut found: HashMap<Uri, String> = HashMap::new();
                    collect_files_under(&listing_dir, "lua", &mut found);
                    controllers.extend(found.into_keys().filter_map(|u| uri_to_file_path(&u)));
                } else {
                    let mut p = module_dir.join(&script);
                    if p.extension().is_none() {
                        p.set_extension("lua");
                    }
                    controllers.push(p);
                }
            }
        }
    }
    controllers.sort();
    controllers.dedup();

    let mut pairs: Vec<(Uri, Uri)> = Vec::new();
    for controller_path in controllers {
        let Some(lua_uri) = uri_from_file_path(&controller_path) else {
            continue;
        };
        let Some(text) = read_indexed_file(&lua_uri) else {
            continue;
        };
        // The controller's OWN directory, not `module_dir` — `LuaInterface::loadScript` resolves a
        // relative argument against `getCurrentSourcePath()`, the directory of whichever `.lua`
        // source is *currently executing* the call, which is only the module's root for a top-level
        // controller. A nested one (`game_cyclopedia/tab/bestiary/bestiary.lua`, real corpus example)
        // resolves `g_ui.loadUI("bestiary")` against `tab/bestiary/`, not `game_cyclopedia/` —
        // resolving against `module_dir` here would look for a `game_cyclopedia/bestiary.otui` that
        // does not exist, and silently miss the pairing entirely.
        let controller_dir = controller_path.parent().unwrap_or(module_dir);
        for load in scan_ui_loads(&text) {
            if let Some(rest) = load.path.strip_prefix('/') {
                // A leading `/` resolves against the mounted virtual filesystem root (the OTClient
                // install's `mods`/`modules`/`data` overlay, then the bare root itself) — the same
                // rule `resolve_asset_candidates` already applies to ordinary `image-source`/`icon`
                // asset paths (see [`resolve_vfs_rooted_otui`]'s doc comment for the engine trace).
                // This is still an EXACT resolution, not a heuristic: the path is a complete literal
                // naming one specific file, so pairing to whatever it resolves to is correct even
                // when that file lives in a wholly different module's directory (real corpus example:
                // `game_taskboard/trackers/miniwindows_tracker.lua`'s
                // `g_ui.importStyle('/modules/game_taskboard/trackers/styles/kill_tracker.otui')`
                // resolves under its OWN module, but the mechanism is the same one that would resolve
                // a genuinely cross-module target). With no client root detected for this module,
                // there is no trustworthy mount set to resolve against — skip rather than guess,
                // mirroring [`detect_client_roots`]'s own silence rule.
                if let Some(otui) = resolve_vfs_rooted_otui(rest, client_roots) {
                    let Some(otui_uri) = uri_from_file_path(&otui) else {
                        continue;
                    };
                    pairs.push((lua_uri.clone(), otui_uri));
                }
                continue;
            }
            if Path::new(&load.path)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                // A `..` component would let a complete-literal `g_ui.loadUI('../otherModule/ui')`
                // resolve, through a plain `Path::join` + `is_file()` check, straight into a
                // FOREIGN module's directory — a false controller<->UI pairing. The real engine
                // never does that walk-up: `resolvePath` does not collapse `..`, and PhysFS (the
                // virtual filesystem OTClient mounts `data`/`modules` through) rejects a path
                // containing one outright, so the engine itself would never load such a target.
                // Zero occurrences of a *complete-literal* `..` argument in the real corpus today
                // (every `..` there comes from a runtime string concatenation, which
                // `scan_ui_loads` already drops as a non-literal argument) — this is a defensive
                // guard against the worst-outcome class this whole mechanism exists to avoid, not a
                // fix for an observed false pairing.
                continue;
            }
            let mut p = controller_dir.join(&load.path);
            if p.extension().is_none() {
                p.set_extension("otui");
            }
            if !p.is_file() {
                continue; // best-effort: never pair against a target that does not exist on disk
            }
            let Some(otui_uri) = uri_from_file_path(&p) else {
                continue;
            };
            pairs.push((lua_uri.clone(), otui_uri));
        }
    }
    pairs.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
    pairs.dedup();
    pairs
}

/// Resolve a `loadUI`/`displayUI`/`importStyle` argument's `rest` (the literal with its leading `/`
/// already stripped) against the mounted OTClient virtual filesystem — the SAME mount set
/// [`resolve_asset_candidates`] probes for an ordinary `/`-rooted asset path: each of `client_roots`
/// joined with [`otui_core::links::ASSET_MOUNT_DIRS`] (`mods`/`modules`/`data`, highest priority
/// first), then the bare root itself (the always-mounted, lowest-priority search path). `.otui` is
/// implied when `rest` has no extension of its own, mirroring the plain-relative branch just above
/// the call site in [`scan_module_dir`]. Returns the first candidate that exists on disk, or `None`
/// when nothing under any root does — never a guess.
///
/// Confirmed against the engine's own path resolution (`ResourceManager::resolvePath`,
/// `resourcemanager.cpp`):
/// ```cpp
/// if (path.starts_with("/"))
///     fullPath = path;
/// else {
///     ...
///     fullPath += scriptPath + "/"; // getCurrentSourcePath() — the *relative* branch
///     fullPath += path;
/// }
/// ```
/// — a leading `/` is used exactly as written, never joined onto the calling script's own
/// directory; the resulting `fullPath` is then looked up through PHYSFS, which searches every
/// mounted path (the install root itself, plus `mods`/`modules`/`data` — see
/// [`otui_core::links::ASSET_MOUNT_DIRS`]'s doc comment for the full `init.lua`/`main.cpp` mount
/// trace) in mount order, first match wins.
fn resolve_vfs_rooted_otui(rest: &str, client_roots: &[PathBuf]) -> Option<PathBuf> {
    fn with_otui_extension(mut p: PathBuf) -> PathBuf {
        if p.extension().is_none() {
            p.set_extension("otui");
        }
        p
    }

    for root in client_roots {
        for mount in otui_core::links::ASSET_MOUNT_DIRS {
            let candidate = with_otui_extension(root.join(mount).join(rest));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let candidate = with_otui_extension(root.join(rest));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Build [`ModuleUiIndex`] from scratch by scanning every module directory under `roots`. Blocking
/// filesystem work, run only from the background scan thread — mirrors
/// [`scan_workspace`]/[`scan_workspace_lua`] in that respect.
///
/// Each module directory's `/`-rooted `loadUI` targets are resolved against the client root(s)
/// detected FOR THAT DIRECTORY ([`detect_client_roots`], anchored on the module directory itself, the
/// same anchoring [`missing_asset_diagnostics`] uses per-document) — not a single client root shared
/// across the whole workspace scan — so a workspace containing more than one OTClient checkout still
/// resolves each module against its own install, never a foreign one.
fn build_module_index(roots: &[Uri]) -> ModuleUiIndex {
    let workspace_roots: Vec<PathBuf> = roots.iter().filter_map(uri_to_file_path).collect();
    let mut index = ModuleUiIndex::new();
    for dir in find_module_dirs(roots) {
        let client_roots = detect_client_roots(Some(&dir), &workspace_roots);
        let pairs = scan_module_dir(&dir, &client_roots);
        if !pairs.is_empty() {
            index.by_module.insert(dir, pairs);
        }
    }
    index.rebuild_derived();
    index
}

/// The two subdirectories that, together with `init.lua` itself, confirm a directory is the real
/// OTClient **install root** — the `getWorkDir()` `init.lua` mounts `data/`, `modules/` and `mods/`
/// under (see [`otui_core::links::ASSET_MOUNT_DIRS`]'s doc comment for the full `init.lua` trace).
/// `mods/` is deliberately not required here: it is created lazily by the client and is commonly
/// absent from a freshly cloned tree, while `init.lua` + `data/` + `modules/` are always present
/// together at the repository root.
const CLIENT_ROOT_MARKERS: [&str; 2] = ["data", "modules"];

/// Walk up from `start` (inclusive of `start` itself) looking for the real OTClient install root — a
/// directory containing `init.lua` plus both of [`CLIENT_ROOT_MARKERS`] as subdirectories. Returns
/// the first (innermost) such ancestor, or `None` if the walk reaches the filesystem root without
/// finding one.
///
/// This exists because **the workspace root the editor opened is not the client root** (Finding 2):
/// `/`-rooted OTUI asset paths are relative to `getWorkDir()`, the client install's own directory —
/// which has nothing to do with the folder a developer happened to open. A workspace opened on a
/// single module or mod repository (the ordinary unit of distribution) has no `data/`/`modules/`
/// siblings at all, so joining a `/`-rooted path directly onto that root (the pre-fix behavior) does
/// not approximate the engine — it guesses, and guesses wrong for the overwhelming majority of
/// `/`-rooted paths in that shape of workspace.
fn find_client_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("init.lua").is_file() && CLIENT_ROOT_MARKERS.iter().all(|m| d.join(m).is_dir()) {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Detect the OTClient install root(s) that should back a `/`-rooted asset path's resolution.
///
/// When `doc_dir` itself sits inside (or at) a confirmed install root, **only that root** is
/// returned — a client installation reachable from some *other*, unrelated workspace root is never
/// folded in. Mixing them together let an asset that happens to exist under an unrelated client
/// install elsewhere in the workspace suppress a legitimate `missing-asset` finding for a document
/// that has nothing to do with that install, or make hover/document-link resolve against the wrong
/// tree entirely (CodeRabbit Finding 1 on PR #51) — a workspace can legitimately contain more than
/// one OTClient checkout (e.g. two client forks side by side), and each document's assets must
/// resolve against *its own* install, not whichever one the scan happened to find first.
///
/// Only when `doc_dir` has **no** client root anywhere above it does this fall back to scanning every
/// `workspace_roots` entry for a client root of its own (deduplicated) — the pre-existing behavior,
/// still needed for the request paths that have no single document to anchor on (the workspace-scan
/// completion refresh; see its caller in `run_initialized`). Empty when no client root is found
/// anywhere — the signal [`missing_asset_diagnostics`] uses to stay silent instead of guessing
/// (Finding 2, the original one).
///
/// `pub` for the same reason as [`otui_core::links::is_runtime_variable_path`]: `xtask corpus` reuses
/// this guard, so its `missing-asset` count cannot silently drift from what the real diagnostic would
/// report.
#[must_use]
pub fn detect_client_roots(doc_dir: Option<&Path>, workspace_roots: &[PathBuf]) -> Vec<PathBuf> {
    if let Some(root) = doc_dir.and_then(find_client_root) {
        return vec![root];
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for start in workspace_roots {
        if let Some(root) = find_client_root(start)
            && !out.contains(&root)
        {
            out.push(root);
        }
    }
    out
}

/// Whether a `*.otpkg` archive exists anywhere under `dir` (recursive; stops at the first match).
///
/// OTClient mounts every `.otpkg` under the data root as a transparent virtual-filesystem overlay at
/// startup (`init.lua`'s `g_resources.searchAndAddPackages('/', '.otpkg', true)` →
/// `ResourceManager::searchAndAddPackages` → `addSearchPath` → `PHYSFS_mount`,
/// `resourcemanager.cpp`), and every engine existence check goes through `PHYSFS_exists`
/// (`ResourceManager::fileExists`), never the raw OS filesystem. An asset shipped inside one is
/// therefore invisible to our `Path::is_file()` probe — a guaranteed false positive we cannot
/// resolve without unpacking the archive (out of scope; not attempted). The real engine call only
/// lists the *immediate* entries of the mounted virtual root (not a deep walk) — this walk is
/// deliberately broader (any depth, any subdirectory) than that single call, since suppressing too
/// eagerly is the safe direction for a diagnostic and a shallower check could still miss an `.otpkg`
/// sitting one level differently than the engine's exact listing.
///
/// Also documents a hole this crate cannot close statically: `addSearchPath` is itself bound to Lua
/// (`g_resources.addSearchPath`, `src/framework/luafunctions.cpp`), so a module can mount an
/// arbitrary extra root *at runtime* — no static analysis of `.otui`/`.lua` text can predict that.
///
/// Only called once [`missing_asset_diagnostics`] already has a real candidate finding to confirm or
/// suppress, so this recursive walk is never paid for the overwhelmingly common case where nothing
/// would be flagged anyway. Recursive and uncached in itself — see [`Backend::otpkg_cache`] for why
/// the *server's* diagnostics path never actually re-walks the same root twice (Finding 3).
///
/// `pub` for the same reason as [`detect_client_roots`]: `xtask corpus` reuses this guard.
#[must_use]
pub fn otpkg_present_under(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if otpkg_present_under(&path) {
                return true;
            }
        } else if meta.is_file() && path.extension().is_some_and(|e| e == "otpkg") {
            return true;
        }
    }
    false
}

/// Memoized [`otpkg_present_under`] lookup, keyed by client root.
///
/// [`missing_asset_diagnostics`] re-derives the same install root(s) on **every** diagnostics pass
/// (every keystroke in an open document with at least one missing asset), and the diagnostics loop
/// runs synchronously on the single-threaded LSP main loop — without this memoization,
/// `otpkg_present_under`'s recursive walk of the *entire* client tree would be repeated on every one
/// of those passes, freezing typing on a large client tree (CodeRabbit Finding 3 on PR #51; a prior
/// measurement found ~4ms for a 62MB engine tree, and a full asset-laden client install is far
/// bigger).
///
/// Populated lazily, once per distinct root, and kept for the life of the [`Backend`]: `.otpkg`
/// archives are shipped, packaged build artifacts, not files a developer edits during a session, and
/// — unlike `.otui`/`.lua` — nothing watches for `.otpkg` changes (`run_initialized`'s dynamic
/// watcher registration only asks for `**/*.otui` and `**/*.lua`), so there is no live-invalidation
/// signal to hook this into. A server restart re-derives it fresh. This is the smaller of the two
/// fixes Finding 3 offered ("compute once … and store it on the `Backend`"): the walk itself is
/// unchanged, only ever repeated once per root instead of once per keystroke.
fn otpkg_present_under_cached(cache: &RwLock<HashMap<PathBuf, bool>>, root: &Path) -> bool {
    if let Some(&present) = cache.read().expect("otpkg_cache lock poisoned").get(root) {
        return present;
    }
    let present = otpkg_present_under(root);
    cache
        .write()
        .expect("otpkg_cache lock poisoned")
        .insert(root.to_path_buf(), present);
    present
}

/// Compute the `missing-asset` diagnostics for `text`: a **warning** (never an error — spec §2.10,
/// the LSP is never stricter than the engine) on every path-valued property whose target does not
/// resolve to a file on disk, via the exact same walk `document_link` uses
/// ([`resolve_asset_candidates`] and the `is_file()` probe) — over `asset_links`, the document's
/// asset-path links the caller already extracted (see
/// [`OtuiService::diagnostics_with_widgets_and_links`]; not re-parsed here, so this document is
/// never parsed twice for one diagnostics pass).
///
/// Guards that keep this from being a false-positive machine (the whole risk of the rule):
/// * a path containing `$` is a runtime-resolved OTML variable ([`is_runtime_variable_path`]) —
///   never diagnosed, since the server cannot know what it resolves to;
/// * an explicit "no image" sentinel value on `image-source` specifically ([`is_asset_sentinel_value`]:
///   `""` / `none` / an inline `base64:` image, after the engine's double-quote stripping) is not a
///   broken reference — never diagnosed. `icon`/`icon-source` have no such engine-verified
///   short-circuit and are never treated as sentinels, however they are spelled — see
///   [`is_asset_sentinel_value`]'s doc comment;
/// * with **no** `doc_dir` (a non-`file:` document) — nothing is resolvable, so nothing is
///   diagnosed;
/// * with **no detected OTClient install root** ([`detect_client_roots`]) — a `/`-rooted path has no
///   trustworthy data root to resolve against (the workspace root the editor opened is *not* the
///   client root; see [`find_client_root`]'s doc comment), so nothing is claimed missing at all
///   (Finding 2 — silence, not noise, exactly like the no-workspace-root case it replaces);
/// * a workspace containing any mounted `*.otpkg` archive ([`otpkg_present_under_cached`]) — an
///   asset shipped inside one is invisible to the raw filesystem probe, so the whole document's
///   findings are suppressed rather than risk flagging an asset that does exist, just not where we
///   can see it (Finding 3).
fn missing_asset_diagnostics(
    asset_links: Vec<PathRef>,
    text: &str,
    doc_dir: Option<&Path>,
    workspace_roots: &[PathBuf],
    otpkg_cache: &RwLock<HashMap<PathBuf, bool>>,
    encoding: PositionEncoding,
) -> Vec<LspDiagnostic> {
    let Some(doc_dir) = doc_dir else {
        return Vec::new();
    };
    let client_roots = detect_client_roots(Some(doc_dir), workspace_roots);
    if client_roots.is_empty() {
        return Vec::new();
    }

    let missing: Vec<PathRef> = asset_links
        .into_iter()
        .filter(|link| !is_runtime_variable_path(&link.path))
        .filter(|link| !is_asset_sentinel_value(link.key, &link.path))
        .filter(|link| {
            !resolve_asset_candidates(&link.path, doc_dir, &client_roots)
                .into_iter()
                .any(|candidate| candidate.is_file())
        })
        .collect();
    if missing.is_empty() {
        return Vec::new();
    }
    if client_roots
        .iter()
        .any(|root| otpkg_present_under_cached(otpkg_cache, root))
    {
        return Vec::new();
    }

    let index = LineIndex::new(text);
    missing
        .into_iter()
        .map(|link| LspDiagnostic {
            range: index.range(link.span.start, link.span.end, encoding),
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("missing-asset".to_owned())),
            code_description: None,
            source: Some(convert::DIAGNOSTIC_SOURCE.to_owned()),
            message: format!("Asset not found on disk: {}", link.path),
            related_information: None,
            tags: None,
            data: None,
        })
        .collect()
}

/// Read an indexable file (`.otui` or `.lua`) from disk, or `None` when it cannot / should not be
/// indexed: a non-`file:` URI, an unreadable path, a file larger than [`MAX_INDEXED_FILE_BYTES`], or
/// content that is not valid UTF-8 (a binary/garbage file must never crash the server or land bogus
/// entries in an index). The single disk-read seam shared by the scan, the watcher and `did_close`.
fn read_indexed_file(uri: &Uri) -> Option<String> {
    let path = uri_to_file_path(uri)?;
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_INDEXED_FILE_BYTES {
        return None;
    }
    // `read_to_string` fails on non-UTF-8 bytes, which cleanly rejects binary files.
    std::fs::read_to_string(&path).ok()
}

/// Recursively collect every `*.otui` file under `roots`, reading each into `(url, text)` — the
/// `.otui` style corpus for the initial workspace scan.
fn scan_workspace(roots: &[Uri]) -> Vec<(Uri, String)> {
    scan_workspace_ext(roots, "otui")
}

/// Recursively collect every `*.lua` file under `roots`, reading each into `(url, text)` — the Lua
/// module corpus scanned for widget definitions ([`OtuiService::lua_widgets`]).
fn scan_workspace_lua(roots: &[Uri]) -> Vec<(Uri, String)> {
    scan_workspace_ext(roots, "lua")
}

/// Recursively collect every file with extension `ext` under `roots`, reading each into
/// `(url, text)`.
///
/// Blocking filesystem work — run on the dedicated scan thread spawned in `run_initialized`, never
/// on the single-threaded main loop. Symlinks are **not** followed (so the walk cannot escape the
/// root or loop), unreadable directories are skipped, and each file is read through
/// [`read_indexed_file`] (so oversized/binary files are dropped). Duplicate roots (or nested roots)
/// are de-duplicated by URL at the end.
fn scan_workspace_ext(roots: &[Uri], ext: &str) -> Vec<(Uri, String)> {
    let mut out: HashMap<Uri, String> = HashMap::new();
    for root in roots {
        let Some(dir) = uri_to_file_path(root) else {
            continue;
        };
        collect_files_under(&dir, ext, &mut out);
    }
    out.into_iter().collect()
}

/// Depth-first walk of `dir`, pushing every readable file whose extension is `ext` into `out` keyed
/// by its `file://` URL. Does not follow symlinks (checked via the dir entry's own metadata) and
/// silently skips entries it cannot stat/read.
fn collect_files_under(dir: &Path, ext: &str, out: &mut HashMap<Uri, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path: PathBuf = entry.path();
        // `symlink_metadata` does not traverse the link, so a symlink is classified as a symlink and
        // skipped — the walk cannot follow one out of the root or into a cycle.
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_files_under(&path, ext, out);
        } else if meta.is_file()
            && path.extension().is_some_and(|e| e == ext)
            && let Some(uri) = uri_from_file_path(&path)
            && let Some(text) = read_indexed_file(&uri)
        {
            out.insert(uri, text);
        }
    }
}

/// The workspace roots carried by an `initialize` request: `workspace_folders` when present (each
/// folder's URI), else the single legacy `root_uri`, else empty. Empty means the client opened no
/// folder, and the server falls back to open-docs-only indexing.
#[allow(deprecated)] // `InitializeParams.root_uri` is the mandatory legacy fallback; still read.
fn workspace_roots(params: &InitializeParams) -> Vec<Uri> {
    if let Some(folders) = &params.workspace_folders
        && !folders.is_empty()
    {
        return folders.iter().map(|f| f.uri.clone()).collect();
    }
    params.root_uri.clone().into_iter().collect()
}

/// Build an LSP [`Location`] for `span` in the document identified by `doc_id`.
///
/// A style def's spans are byte offsets into **its own** document's text, so the range must be
/// mapped against that text. `documents` is the merged open+disk view (see [`merge_documents`]), so
/// both open buffers and indexed closed files resolve here. Returns `None` — and the caller skips the
/// entry — when `doc_id` is not a parseable URL or its text is in neither view (its span cannot be
/// mapped to a range). Shared by [`resolve_base_definition`] (go-to-definition) and
/// [`collect_workspace_symbols`] (workspace symbols).
fn resolve_location(
    doc_id: &DocId,
    span: ByteSpan,
    documents: &HashMap<Uri, Document>,
    encoding: PositionEncoding,
) -> Option<Location> {
    let target_uri = Uri::from_str(doc_id.as_str()).ok()?;
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
    documents: &HashMap<Uri, Document>,
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
/// [`style_type_at`](OtuiService::style_type_at)) — is looked up in the cached workspace index
/// ([`StyleIndex::lookup`]); each declaration's `name_span` becomes an LSP [`Location`] built against
/// **that** target document's own text. Answering from the cache avoids reparsing every open document.
/// A native `UI*` type has no user declaration in the index and so resolves to `None`, exactly like a
/// native base in go-to-definition. Duplicate declarations (legal) each become a location — zero is
/// `None`, one a `Scalar`, several an `Array`. This is the same shape as [`resolve_base_definition`].
///
/// Kept as a free function over borrowed state so it can be unit-tested without a live `Client`.
fn resolve_type_definition(
    index: &StyleIndex,
    documents: &HashMap<Uri, Document>,
    name: &str,
    encoding: PositionEncoding,
) -> Option<GotoDefinitionResponse> {
    let mut locations = Vec::new();
    for (doc_id, def) in index.lookup(name) {
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

/// Collect the styles that derive from `name` for `textDocument/implementation` (spec §5.2).
///
/// Reads the styles whose base is `name` from the cached workspace index ([`StyleIndex::subtypes`] —
/// every top-level `X < name` header) and maps each one's `name_span` to a [`Location`] against that
/// target document's own text. Answering from the cache avoids reparsing every open document. The
/// style namespace is global, so this spans the whole workspace. Unlike typeDefinition, a native `UI*`
/// name is *not* suppressed: user styles commonly derive from a native base, and listing those
/// derivations is exactly the point. Returns an empty vector when nothing derives from `name`; the
/// handler maps empty to `None`.
///
/// Kept as a free function over borrowed state so it can be unit-tested without a live `Client`
/// (mirroring [`resolve_base_definition`] / [`collect_references`]).
fn collect_implementations(
    index: &StyleIndex,
    documents: &HashMap<Uri, Document>,
    name: &str,
    encoding: PositionEncoding,
) -> Vec<Location> {
    let mut out = Vec::new();
    for (doc_id, def) in index.subtypes(name) {
        if let Some(loc) = resolve_location(doc_id, def.name_span, documents, encoding) {
            out.push(loc);
        }
    }
    out
}

/// Build a [`TypeHierarchyItem`] for the style `def` declared in `doc_id` (spec: type hierarchy).
///
/// A style is modelled as a [`SymbolKind::CLASS`] whose `range` is the whole `header_span` (the
/// declaration and any indented body) and whose `selection_range` is the `name_span` (the declared
/// name identifier) — both byte spans into **that** document's own text, so the ranges are mapped
/// against it. `detail` carries the style's base (like the hover's "inherits from"), and `data`
/// round-trips the style **name** as a JSON string so a later `supertypes`/`subtypes` request can
/// recover the exact style the item stands for (see [`item_style_name`]).
///
/// Returns `None` — and the caller skips the entry — when `doc_id` is not a parseable URL or its
/// text is in neither the open nor the disk view (its spans cannot be mapped to ranges). Mirrors
/// [`resolve_location`]'s span→range mapping, and is kept `Client`-free so it is unit-testable
/// without a live server.
fn build_type_hierarchy_item(
    doc_id: &DocId,
    def: &StyleDef,
    documents: &HashMap<Uri, Document>,
    encoding: PositionEncoding,
) -> Option<TypeHierarchyItem> {
    let uri = Uri::from_str(doc_id.as_str()).ok()?;
    let doc = documents.get(&uri)?;
    let line_index = LineIndex::new(&doc.text);
    Some(TypeHierarchyItem {
        name: def.name.clone(),
        kind: SymbolKind::CLASS,
        tags: None,
        detail: def.base.clone(),
        uri,
        range: line_index.range(def.header_span.start, def.header_span.end, encoding),
        selection_range: line_index.range(def.name_span.start, def.name_span.end, encoding),
        data: Some(serde_json::Value::String(def.name.clone())),
    })
}

/// The style name a [`TypeHierarchyItem`] stands for, read back from what
/// [`build_type_hierarchy_item`] stored.
///
/// Prefers the `data` field (a JSON string carrying the style name, preserved across the
/// prepare→supertypes/subtypes round-trip), falling back to the item's `name` when `data` is absent
/// or not a string. This is what the `supertypes`/`subtypes` graph queries key off, so an item the
/// server built always resolves back to the right style.
fn item_style_name(item: &TypeHierarchyItem) -> String {
    item.data
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| item.name.clone(), str::to_owned)
}

/// Root a type hierarchy on the style named `name` (spec: `textDocument/prepareTypeHierarchy`).
///
/// Looks `name` up in the cached workspace index and builds a [`TypeHierarchyItem`] from its
/// declaration. Returns `None` when `name` has no user declaration — a native `UI*` name or any name
/// absent from the index — since there is nothing to root a hierarchy on (mirroring
/// [`resolve_type_definition`]'s native-is-nothing rule). When a name is declared more than once
/// (legal in the engine), the hierarchy is rooted on the **first** declaration the index yields that
/// maps to an open document (the backing map is unordered, so "first" is stable only per index
/// state); the client can still walk supertypes/subtypes from it.
///
/// Kept `Client`-free so it is unit-testable without a live server (mirroring
/// [`resolve_base_definition`]).
fn prepare_type_hierarchy_item(
    index: &StyleIndex,
    documents: &HashMap<Uri, Document>,
    name: &str,
    encoding: PositionEncoding,
) -> Option<TypeHierarchyItem> {
    index
        .lookup(name)
        .into_iter()
        .find_map(|(doc_id, def)| build_type_hierarchy_item(doc_id, def, documents, encoding))
}

/// The direct supertype(s) of the style `name` (spec: `typeHierarchy/supertypes`) — its base.
///
/// One level only: the client walks further up by calling supertypes again on the returned item. The
/// direct supertype of `name` is its **base** (from `name`'s declaration in the index). A base that
/// is a **user style** present in the index yields a [`TypeHierarchyItem`] built from the base's own
/// declaration; a **native `UI*`** base, an absent base, or a base with no declaration in the index
/// yields nothing — native classes are built-in leaves with no navigable declaration, so the chain
/// ends there (an empty list is the LSP "no supertypes" answer). Each distinct base is emitted once.
///
/// Kept `Client`-free so it is unit-testable without a live server.
fn resolve_supertypes(
    index: &StyleIndex,
    documents: &HashMap<Uri, Document>,
    name: &str,
    encoding: PositionEncoding,
) -> Vec<TypeHierarchyItem> {
    // Gather the distinct base names of every declaration of `name` (duplicates may differ in base).
    let mut bases: Vec<&str> = Vec::new();
    for (_doc_id, def) in index.lookup(name) {
        if let Some(base) = def.base.as_deref() {
            // A native `UI*` base is a built-in leaf: the chain ends, so it is not a navigable
            // supertype.
            if !is_native_base(base) && !bases.contains(&base) {
                bases.push(base);
            }
        }
    }

    let mut out = Vec::new();
    for base in bases {
        for (doc_id, def) in index.lookup(base) {
            if let Some(item) = build_type_hierarchy_item(doc_id, def, documents, encoding) {
                out.push(item);
            }
        }
    }
    out
}

/// The direct subtypes of the style `name` (spec: `typeHierarchy/subtypes`) — the styles deriving
/// from it.
///
/// One level only: the client walks further down by calling subtypes again on each returned item.
/// Reads the styles whose base is `name` from the cached index ([`StyleIndex::subtypes`], every
/// top-level `X < name` across the whole workspace — the namespace is global) and builds a
/// [`TypeHierarchyItem`] from each. An empty list means nothing derives from `name`.
///
/// Kept `Client`-free so it is unit-testable without a live server.
fn resolve_subtypes(
    index: &StyleIndex,
    documents: &HashMap<Uri, Document>,
    name: &str,
    encoding: PositionEncoding,
) -> Vec<TypeHierarchyItem> {
    let mut out = Vec::new();
    for (doc_id, def) in index.subtypes(name) {
        if let Some(item) = build_type_hierarchy_item(doc_id, def, documents, encoding) {
            out.push(item);
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
/// * A [`StyleName`](ReferenceTarget::StyleName) fans out across **every** document in the merged
///   open+disk view (the style namespace is global, and closed workspace files are indexed too):
///   each document's declarations (only when `include_declaration`) and base
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
    current_uri: &Uri,
    documents: &HashMap<Uri, Document>,
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
            if include_declaration && let Some(span) = occ.declaration {
                out.push(convert::location_of(
                    current_uri.clone(),
                    &doc.text,
                    span,
                    encoding,
                ));
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

/// Collect the LSP [`DocumentHighlight`]s answering a `textDocument/documentHighlight` request for the
/// symbol under the cursor (spec §5.4), scanning **only** `text` (the current document).
///
/// This is the document-local cousin of [`collect_references`]: it reuses the very same occurrence
/// finders (`style_name_occurrences` / `id_occurrences`), but never fans out across the workspace and
/// never consults the [`StyleIndex`] — a highlight only colors occurrences in the buffer the cursor is
/// in. Both a style name (its top-level declaration(s) + every base ref) and an `id:` (its declaration
/// + every `<id>.edge` anchor ref) are handled.
///
/// Kind coloring is the idiomatic read/write split: the **declaration** span (which *defines* the
/// symbol) is [`DocumentHighlightKind::WRITE`]; every usage (base ref / anchor ref) is
/// [`DocumentHighlightKind::READ`].
///
/// Kept as a free function over borrowed state (no `Client`, no lock) so it is unit-testable in
/// isolation, mirroring [`collect_references`].
fn collect_document_highlights(
    target: &ReferenceTarget,
    text: &str,
    service: &OtuiService,
    encoding: PositionEncoding,
) -> Vec<DocumentHighlight> {
    let line_index = LineIndex::new(text);
    let mut out = Vec::new();
    let mut push = |span: ByteSpan, kind: DocumentHighlightKind| {
        out.push(DocumentHighlight {
            range: line_index.range(span.start, span.end, encoding),
            kind: Some(kind),
        });
    };
    match target {
        ReferenceTarget::StyleName(name) => {
            let occ = service.style_name_occurrences(text, name);
            // The declaration defines the symbol → WRITE; base references read it → READ.
            for span in occ.declarations {
                push(span, DocumentHighlightKind::WRITE);
            }
            for span in occ.base_refs {
                push(span, DocumentHighlightKind::READ);
            }
        }
        ReferenceTarget::Id(id) => {
            let occ = service.id_occurrences(text, id);
            // The `id:` declaration defines the id → WRITE; anchor references read it → READ.
            if let Some(span) = occ.declaration {
                push(span, DocumentHighlightKind::WRITE);
            }
            for span in occ.anchor_refs {
                push(span, DocumentHighlightKind::READ);
            }
        }
    }
    out
}

/// The error message returned when a `textDocument/rename` carries a `new_name` that is not a valid
/// OTML identifier (spec §rename). Rewriting occurrences with a name the grammar could not re-parse
/// would silently corrupt the document, so a bad rename is rejected rather than applied. The
/// dispatch arm turns this message into a JSON-RPC `InvalidParams` [`Response`].
fn invalid_identifier_message(new_name: &str) -> String {
    format!(
        "`{new_name}` is not a valid OTML name: it must be non-empty, start with a letter or `_`, \
         and contain only letters, digits, `_` or `-`."
    )
}

/// Build the [`WorkspaceEdit`] that renames `target` to `new_name` (spec §rename), or `None` when
/// there is nothing to rename.
///
/// * **Validation.** `new_name` must be a valid OTML identifier (grammar `IDENT`, via
///   [`is_valid_identifier`](otui_core::schema::is_valid_identifier)); otherwise an `Err(message)`
///   is returned (the dispatch arm maps it to a JSON-RPC `InvalidParams` error) — a broken name must
///   never be written into the document.
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
    current_uri: &Uri,
    documents: &HashMap<Uri, Document>,
    index: &StyleIndex,
    service: &OtuiService,
    new_name: &str,
    encoding: PositionEncoding,
) -> Result<Option<WorkspaceEdit>, String> {
    if !otui_core::schema::is_valid_identifier(new_name) {
        return Err(invalid_identifier_message(new_name));
    }

    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
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
/// to a [`Uri`] and builds a [`Location`] for its `name_span` against
/// **that** target document's own text (via [`convert::location_of`]), exactly as
/// [`resolve_base_definition`] does. A def whose document is in neither the open nor the disk view is
/// skipped — its span cannot be mapped to a range. The widget's base
/// becomes the entry's `container_name`, giving the picker useful context; native `UI*` bases are
/// never symbols of their own (they have no def, so are absent from the index) — they surface only as
/// the `container_name` of a widget that inherits them.
///
/// Duplicate style names (legal in the engine) each produce their own entry; nothing is deduped.
/// Kept as a free function over borrowed state so it can be unit-tested without a live `Client`.
#[allow(deprecated)] // `SymbolInformation.deprecated` is a mandatory-but-deprecated struct field.
fn collect_workspace_symbols(
    index: &StyleIndex,
    documents: &HashMap<Uri, Document>,
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

/// Format a [`PropertyHover`] (a property-key description from the engine) into an LSP Markdown
/// [`Hover`]. Pure presentation: [`otui_core`] decided the property's value kind from its catalog +
/// schema metadata; here we only word it and map the key span to a range.
fn render_property_hover(
    desc: &PropertyHover,
    line_index: &LineIndex,
    encoding: PositionEncoding,
) -> Hover {
    let name = &desc.name;
    // Prefer the curated behavior sentence; fall back to a value-kind description when the property
    // is known but outside the curated canonical set.
    let title = match desc.doc {
        Some(doc) => format!("**`{name}`** — {doc}"),
        None => {
            let body = match &desc.value {
                PropertyValueKind::Color => "a color value",
                PropertyValueKind::AssetPath => {
                    "an asset path (a texture) — the `.png` extension is optional"
                }
                PropertyValueKind::Enum { .. } => "one of a fixed value set (see below)",
                PropertyValueKind::Border => "a border shorthand: a width and a color (or `none`)",
                PropertyValueKind::Plain => "an OTUI style property",
            };
            format!("**`{name}`** — {body}")
        }
    };
    let mut value = title;
    // For a fixed-value-set property (display, layout), always append the full accepted list.
    if let PropertyValueKind::Enum { values } = &desc.value {
        let list = values
            .iter()
            .map(|v| format!("`{v}`"))
            .collect::<Vec<_>>()
            .join(", ");
        value.push_str(&format!("\n\nOne of: {list}"));
    }
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(line_index.range(desc.span.start, desc.span.end, encoding)),
    }
}

/// Format a [`PathRef`] (an asset path *value* under the cursor) into an LSP Markdown [`Hover`] —
/// the sprite-preview hover. `resolved` is the on-disk target file, already found by the caller via
/// [`resolve_asset_candidates`] (the same resolution `document_link` and the missing-asset
/// diagnostic use); `None` when it could not be resolved, in which case only the path text is
/// shown — no image, no "not found" note (that claim belongs to the missing-asset diagnostic, which
/// unlike this hover only fires with a workspace root to back it).
///
/// **Client-dependent rendering, by design**: this embeds the resolved file as Markdown image
/// syntax (`![](<file:///…>)`). Whether the connected editor actually renders that inline (as
/// opposed to a plain link, or nothing) is entirely up to the client's Markdown renderer — this
/// crate has no way to verify it, and no test here proves an image appears on screen. The
/// `![]( … )` line is isolated to the branch below so swapping it for a plain clickable link
/// (`[Open asset](<file:///…>)`) — the safe fallback if a target client does not render inline
/// images from hover Markdown — is a one-line change.
///
/// **The destination is wrapped in `<…>`** (CommonMark's angle-bracket link-destination form),
/// deliberately: `url::Url::from_file_path` does not percent-encode `(`/`)` (they are RFC 3986
/// sub-delims, outside the WHATWG path percent-encode set — confirmed empirically, not assumed), so
/// a raw `![](file:///…)` destination closes early at the first `)` in the path — whether that `)`
/// comes from a hostile file name in a cloned repository, or from something as ordinary as a
/// workspace living under a directory like `Program Files (x86)`. `)` is legal inside `<…>`, and `<`
/// / `>` themselves *are* percent-encoded by `url`, so the wrapper cannot itself be forged open by
/// the URI's own content. `path_ref.path` (the raw, untrusted document text) is similarly fenced
/// into a code span wide enough that it cannot be closed early — see [`code_span`].
fn render_asset_hover(
    path_ref: &PathRef,
    resolved: Option<&Path>,
    line_index: &LineIndex,
    encoding: PositionEncoding,
) -> Hover {
    let mut value = format!("**Asset** {}", code_span(&path_ref.path));
    if let Some(target_uri) = resolved.and_then(uri_from_file_path) {
        // Sprite preview: an inline image if the client's Markdown renderer supports it (see the
        // doc comment above — unverified here, client-dependent).
        value.push_str(&format!("\n\n![](<{}>)", target_uri.as_str()));
    }
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(line_index.range(path_ref.span.start, path_ref.span.end, encoding)),
    }
}

/// Wrap untrusted `text` in a Markdown inline code span using a backtick fence one character longer
/// than the longest run of backticks `text` itself contains — the CommonMark-mandated way to fence
/// arbitrary text so a literal backtick inside it can never close the span early. `text` here is
/// `path_ref.path`, raw document content: a crafted value like `` x` <b>evil</b> `y `` would
/// otherwise close a fixed single-backtick span after `x` and let the rest render as live Markdown/
/// HTML in the hover. A leading/trailing space is always added around the content, which CommonMark
/// requires when the content itself starts or ends with a backtick (or is empty) and is otherwise
/// harmless (CommonMark trims exactly one such space back off on render).
///
/// Newlines are collapsed to spaces first. A block-scalar value (`image-source: |`) can carry a
/// blank line into `path_ref.path`, and a code span cannot span a blank line — the fence would be
/// left open and everything after the blank line would render as a live paragraph. Backtick fencing
/// alone does not close that hole; flattening to a single line does.
fn code_span(text: &str) -> String {
    let text: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let longest_backtick_run = text
        .as_bytes()
        .split(|&b| b != b'`')
        .map(<[u8]>::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_backtick_run + 1);
    format!("{fence} {text} {fence}")
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
    uri: &Uri,
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

/// Whether the client can consume a snippet `insert_text` (`$0`/`$1` tab-stops, `${1:placeholder}`).
/// Per LSP 3.17, a client signals this via
/// `textDocument.completion.completionItem.snippetSupport`; when the capability is absent the
/// default is `false` — a client that never opted in has no tab-stop engine, so sending it snippet
/// syntax would paste the placeholders literally into the buffer. See
/// [`convert::completion_item_to_lsp`] for where this is enforced.
fn client_supports_snippets(params: &InitializeParams) -> bool {
    params
        .capabilities
        .text_document
        .as_ref()
        .and_then(|td| td.completion.as_ref())
        .and_then(|c| c.completion_item.as_ref())
        .and_then(|ci| ci.snippet_support)
        .unwrap_or(false)
}

/// Whether the client can be asked to refresh its code lenses via a server-initiated
/// `workspace/codeLens/refresh` request. Per LSP 3.17, a client signals this via
/// `workspace.codeLens.refreshSupport`; when the capability is absent the default is `false` — a
/// client that never opted in gets no such request (see [`Backend::code_lens_refresh_support`]).
fn client_supports_code_lens_refresh(params: &InitializeParams) -> bool {
    params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.code_lens.as_ref())
        .and_then(|c| c.refresh_support)
        .unwrap_or(false)
}

/// The `workspace/inlayHint/refresh` analogue of [`client_supports_code_lens_refresh`]: reads
/// `workspace.inlayHint.refreshSupport`, same default.
fn client_supports_inlay_hint_refresh(params: &InitializeParams) -> bool {
    params
        .capabilities
        .workspace
        .as_ref()
        .and_then(|w| w.inlay_hint.as_ref())
        .and_then(|c| c.refresh_support)
        .unwrap_or(false)
}

/// Build a JSON-RPC [`Response`] for a request whose handler returns a serializable value.
///
/// Extracts the typed params (the `$method` string was already matched, so only a JSON shape
/// mismatch can fail — reported as `InvalidParams`) and wraps the handler's return value in
/// `Response::new_ok`. `$handler` is a closure `|params| -> impl Serialize`.
macro_rules! reply {
    ($req:expr_2021, $method:literal, $ty:ty, $handler:expr_2021) => {{
        let req = $req;
        let fallback_id = req.id.clone();
        match req.extract::<$ty>($method) {
            Ok((id, params)) => {
                let handler = $handler;
                Response::new_ok(id, handler(params))
            }
            Err(ExtractError::JsonError { error, .. }) => Response::new_err(
                fallback_id,
                ErrorCode::InvalidParams as i32,
                error.to_string(),
            ),
            Err(ExtractError::MethodMismatch(_)) => Response::new_err(
                fallback_id,
                ErrorCode::InternalError as i32,
                format!("method mismatch dispatching {}", $method),
            ),
        }
    }};
}

impl Backend {
    /// Dispatch a client→server [`Request`] to the matching sync handler and build its [`Response`].
    /// An unknown method yields a `MethodNotFound` error response. (`initialize`/`shutdown` are
    /// handled by the transport scaffold in `main`, not here.)
    pub fn handle_request(&self, req: Request) -> Response {
        let method = req.method.clone();
        match method.as_str() {
            "textDocument/hover" => {
                reply!(req, "textDocument/hover", HoverParams, |p| self.hover(p))
            }
            "textDocument/definition" => {
                reply!(req, "textDocument/definition", GotoDefinitionParams, |p| {
                    self.goto_definition(p)
                })
            }
            "textDocument/typeDefinition" => reply!(
                req,
                "textDocument/typeDefinition",
                GotoTypeDefinitionParams,
                |p| self.goto_type_definition(p)
            ),
            "textDocument/implementation" => reply!(
                req,
                "textDocument/implementation",
                GotoImplementationParams,
                |p| self.goto_implementation(p)
            ),
            "textDocument/references" => {
                reply!(req, "textDocument/references", ReferenceParams, |p| self
                    .references(p))
            }
            "textDocument/documentHighlight" => reply!(
                req,
                "textDocument/documentHighlight",
                DocumentHighlightParams,
                |p| self.document_highlight(p)
            ),
            "textDocument/documentSymbol" => reply!(
                req,
                "textDocument/documentSymbol",
                DocumentSymbolParams,
                |p| self.document_symbol(p)
            ),
            "workspace/symbol" => reply!(req, "workspace/symbol", WorkspaceSymbolParams, |p| self
                .symbol(p)),
            "textDocument/completion" => {
                reply!(req, "textDocument/completion", CompletionParams, |p| self
                    .completion(p))
            }
            "textDocument/codeAction" => {
                reply!(req, "textDocument/codeAction", CodeActionParams, |p| self
                    .code_action(p))
            }
            "textDocument/formatting" => reply!(
                req,
                "textDocument/formatting",
                DocumentFormattingParams,
                |p| self.formatting(p)
            ),
            "textDocument/rangeFormatting" => reply!(
                req,
                "textDocument/rangeFormatting",
                DocumentRangeFormattingParams,
                |p| self.range_formatting(p)
            ),
            "textDocument/onTypeFormatting" => reply!(
                req,
                "textDocument/onTypeFormatting",
                DocumentOnTypeFormattingParams,
                |p| self.on_type_formatting(p)
            ),
            "textDocument/foldingRange" => {
                reply!(req, "textDocument/foldingRange", FoldingRangeParams, |p| {
                    self.folding_range(p)
                })
            }
            "textDocument/semanticTokens/full" => reply!(
                req,
                "textDocument/semanticTokens/full",
                SemanticTokensParams,
                |p| self.semantic_tokens_full(p)
            ),
            "textDocument/documentColor" => reply!(
                req,
                "textDocument/documentColor",
                DocumentColorParams,
                |p| self.document_color(p)
            ),
            "textDocument/colorPresentation" => reply!(
                req,
                "textDocument/colorPresentation",
                ColorPresentationParams,
                |p| self.color_presentation(p)
            ),
            "textDocument/documentLink" => {
                reply!(req, "textDocument/documentLink", DocumentLinkParams, |p| {
                    self.document_link(p)
                })
            }
            // Code lens / inlay hint (node `lens-hints`), grouped together here to keep the merge
            // with concurrent nodes touching this `match` localized to these two arms.
            "textDocument/codeLens" => {
                reply!(req, "textDocument/codeLens", CodeLensParams, |p| self
                    .code_lens(p))
            }
            "textDocument/inlayHint" => {
                reply!(req, "textDocument/inlayHint", InlayHintParams, |p| self
                    .inlay_hint(p))
            }
            "textDocument/prepareRename" => reply!(
                req,
                "textDocument/prepareRename",
                TextDocumentPositionParams,
                |p| self.prepare_rename(p)
            ),
            "textDocument/prepareTypeHierarchy" => reply!(
                req,
                "textDocument/prepareTypeHierarchy",
                TypeHierarchyPrepareParams,
                |p| self.prepare_type_hierarchy(p)
            ),
            "typeHierarchy/supertypes" => reply!(
                req,
                "typeHierarchy/supertypes",
                TypeHierarchySupertypesParams,
                |p| self.supertypes(p)
            ),
            "typeHierarchy/subtypes" => reply!(
                req,
                "typeHierarchy/subtypes",
                TypeHierarchySubtypesParams,
                |p| self.subtypes(p)
            ),
            // `rename` is the one handler that can fail with a JSON-RPC error (an invalid new name),
            // so it is dispatched by hand rather than through `reply!`.
            "textDocument/rename" => {
                let fallback_id = req.id.clone();
                match req.extract::<RenameParams>("textDocument/rename") {
                    Ok((id, params)) => match self.rename(params) {
                        Ok(edit) => Response::new_ok(id, edit),
                        Err(message) => {
                            Response::new_err(id, ErrorCode::InvalidParams as i32, message)
                        }
                    },
                    Err(ExtractError::JsonError { error, .. }) => Response::new_err(
                        fallback_id,
                        ErrorCode::InvalidParams as i32,
                        error.to_string(),
                    ),
                    Err(ExtractError::MethodMismatch(_)) => Response::new_err(
                        fallback_id,
                        ErrorCode::InternalError as i32,
                        "method mismatch dispatching textDocument/rename".to_owned(),
                    ),
                }
            }
            other => Response::new_err(
                req.id,
                ErrorCode::MethodNotFound as i32,
                format!("unhandled request method: {other}"),
            ),
        }
    }

    /// Dispatch a client→server [`Notification`] to the matching sync handler. Unknown methods are
    /// ignored (per the LSP spec a server may drop notifications it does not implement).
    pub fn handle_notification(&self, note: Notification) {
        match note.method.as_str() {
            // The transport's `initialize_finish` consumes the real `initialized` notification, so
            // `main` feeds a synthetic one through here after the handshake; the handler needs no
            // params.
            "initialized" => self.run_initialized(),
            "textDocument/didOpen" => {
                if let Ok(p) = note.extract::<DidOpenTextDocumentParams>("textDocument/didOpen") {
                    self.did_open(p);
                }
            }
            "textDocument/didChange" => {
                if let Ok(p) = note.extract::<DidChangeTextDocumentParams>("textDocument/didChange")
                {
                    self.did_change(p);
                }
            }
            "textDocument/didClose" => {
                if let Ok(p) = note.extract::<DidCloseTextDocumentParams>("textDocument/didClose") {
                    self.did_close(p);
                }
            }
            "workspace/didChangeWatchedFiles" => {
                if let Ok(p) =
                    note.extract::<DidChangeWatchedFilesParams>("workspace/didChangeWatchedFiles")
                {
                    self.did_change_watched_files(p);
                }
            }
            // `$/cancelRequest` is intentionally unhandled: the single-threaded loop finishes each
            // request before reading the next, so our (fast) requests are never in flight to cancel.
            // Honoring cancellation is a future nicety, safe to skip per the LSP spec / rust-analyzer
            // practice. Any other unknown notification is likewise ignored.
            _ => {}
        }
    }

    /// The `InitializeResult` advertised during the handshake. Encoding, hierarchical-symbol support
    /// and workspace roots were negotiated in [`Backend::new`]; this only builds the capabilities
    /// (identical set to the pre-migration server).
    pub fn initialize_result(&self) -> InitializeResult {
        let encoding = self.encoding();
        InitializeResult {
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
                // Document highlight: every occurrence of the style name / `id:` under the cursor
                // within the CURRENT document only — the document-local cousin of references (spec
                // §5.4).
                document_highlight_provider: Some(OneOf::Left(true)),
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
                // Range formatting: reuse the whole-document formatter but scope the resulting edits
                // to the lines the user selected (spec §8).
                document_range_formatting_provider: Some(OneOf::Left(true)),
                // On-type formatting: auto-indent the line Enter just created, computed lexically
                // (no CST) so it still works on a mid-edit document (spec §8). Only `\n` triggers —
                // there is no dedent trigger character, as a dedent is always a user action.
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".to_string(),
                    more_trigger_character: None,
                }),
                // Completion: the OTML closed sets (spec §6). `$` / `@` / `.` / `!` re-trigger
                // completion as those characters open a `$state` selector, an `@event` key, an
                // `anchors.<edge>` / `<target>.<edge>` dotted position, or a `!`-negated state in a
                // multi-state selector (`$hover !…`); `:` opens the value position of a `key: value`
                // property (offering the `display`/`layout` keyword set or the named-color list).
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "$".to_owned(),
                        "@".to_owned(),
                        ".".to_owned(),
                        "!".to_owned(),
                        ":".to_owned(),
                    ]),
                    ..CompletionOptions::default()
                }),
                // Code actions: quick-fixes for the parse-level diagnostics (spec §7). A plain
                // boolean provider — the fixes are computed on demand per request range.
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                // Document colors: inline swatches at every OTML color value, plus color-picker
                // presentations (spec §2.9). A plain boolean provider — colors are computed on
                // demand per request.
                color_provider: Some(ColorProviderCapability::Simple(true)),
                // Document links: clickable asset paths (`image-source: <path>`, `icon` family).
                // Targets are resolved eagerly and only emitted when the file exists on disk, so
                // `resolve_provider` is `false` — there is no `documentLink/resolve`.
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                // Code lens: "N widgets inherit this style" on a top-level style's declared name
                // (only when N >= 1). Computed eagerly per request, so `resolve_provider` is
                // `false` — there is no `codeLens/resolve`.
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                // Inlay hints: the resolved native `UI*` ancestor after a based widget/style's
                // `Base` token (`Foo < SomeStyle →UIButton`). A plain boolean provider — hints are
                // computed on demand per requested viewport range.
                inlay_hint_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "otui-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        }
    }

    /// Post-handshake work (run once, after `initialize_finish`): register the two dynamic
    /// capabilities and kick off the background workspace scan.
    fn run_initialized(&self) {
        // Type hierarchy (the `Name < Base` graph): this lsp-types (0.94.1) has no static
        // `type_hierarchy_provider` field in `ServerCapabilities`, so the only way to advertise it is
        // dynamic registration. We register **unconditionally** rather than gating on the client's
        // `textDocument.typeHierarchy.dynamicRegistration` flag — neither VS Code nor Neovim sets that
        // flag by default, yet both process an incoming `client/registerCapability` for type
        // hierarchy, so gating on it would make the feature undiscoverable in exactly the clients that
        // matter. A client that genuinely cannot handle the registration replies with an error, which
        // arrives as a `Message::Response` the loop ignores (we do not track the ack). A future
        // lsp-types bump would let us advertise this statically instead.
        self.register_capability(
            "otui-type-hierarchy",
            Registration {
                id: "otui-type-hierarchy".to_owned(),
                method: "textDocument/prepareTypeHierarchy".to_owned(),
                register_options: None,
            },
        );

        // Watch every `.otui` (style corpus), `.lua` (widget-definition corpus) and `.otmod` (module
        // manifest, `module_ui_index`'s association source) in the workspace so all three indexes
        // track files edited/created/deleted on disk outside the editor (or in files the user never
        // opens). Registered dynamically for the same reason as type hierarchy above: it is the
        // portable way to request `workspace/didChangeWatchedFiles`, and (like above) it is
        // fire-and-forget — the client's ack is a `Message::Response` the loop ignores. A client that
        // honors dynamic watcher registration (VS Code, Neovim) then delivers `.lua`/`.otmod` change
        // events to keep the Lua widget/module-association indexes live; one that does not still gets
        // the initial scan below.
        self.register_capability(
            "otui-watched-files",
            Registration {
                id: "otui-watched-files".to_owned(),
                method: "workspace/didChangeWatchedFiles".to_owned(),
                register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers: vec![
                        FileSystemWatcher {
                            glob_pattern: GlobPattern::String("**/*.otui".to_owned()),
                            kind: None, // default: create | change | delete
                        },
                        FileSystemWatcher {
                            glob_pattern: GlobPattern::String("**/*.lua".to_owned()),
                            kind: None,
                        },
                        FileSystemWatcher {
                            glob_pattern: GlobPattern::String("**/*.otmod".to_owned()),
                            kind: None,
                        },
                    ],
                })
                .ok(),
            },
        );

        // Initial workspace scan: index every `.otui` on disk so cross-file features
        // (references/rename/definition/…) see closed files, not only open buffers. Spawned on a
        // dedicated `std::thread` so `run_initialized` returns promptly; it walks the roots and
        // writes into the index incrementally, holding each write lock only per file. With no
        // workspace root (client opened a loose file, not a folder) there is nothing to scan, and the
        // server falls back to open-docs-only indexing.
        let roots = self.roots.lock().expect("roots mutex poisoned").clone();
        if roots.is_empty() {
            self.log(
                MessageType::INFO,
                "otui-lsp: no workspace root; indexing open documents only",
            );
        } else {
            let style_index = Arc::clone(&self.style_index);
            let lua_index = Arc::clone(&self.lua_index);
            let lua_ref_index = Arc::clone(&self.lua_ref_index);
            let lua_texts = Arc::clone(&self.lua_texts);
            let module_ui_index = Arc::clone(&self.module_ui_index);
            let disk_texts = Arc::clone(&self.disk_texts);
            let documents = Arc::clone(&self.documents);
            let reindex_guard = Arc::clone(&self.reindex_guard);
            let shutdown = Arc::clone(&self.shutdown);
            let otpkg_cache = Arc::clone(&self.otpkg_cache);
            let sender = self.sender.clone();
            let encoding = self.encoding();
            // Copied out (plain `bool`s, not the `Mutex`) so the scan thread can gate its own
            // completion refresh without holding `&Backend` — see `send_workspace_refresh`.
            let code_lens_refresh_support = self.code_lens_refresh_support();
            let inlay_hint_refresh_support = self.inlay_hint_refresh_support();
            // The scan thread holds a `Sender` clone solely to refresh open documents once the
            // indexes are complete (see the completion refresh below) — otherwise a document opened
            // mid-scan would keep a stale widget-aware diagnostic until its next edit. To keep
            // shutdown prompt despite the live `Sender` (which would otherwise make
            // `IoThreads::join()` wait for this thread), the indexing loops below check the
            // `shutdown` flag between files and bail; `signal_shutdown` sets it before the backend is
            // dropped, so the thread drops its `Sender` clone and unblocks join. The per-directory
            // walk+read inside `scan_workspace`/`scan_workspace_lua` runs before those checks and is
            // not itself interruptible, but it is bounded (each file capped at MAX_INDEXED_FILE_BYTES,
            // no network), so the residual shutdown wait is a bounded latency, never a hang. Progress
            // is reported on stderr, never the LSP channel.
            std::thread::spawn(move || {
                let entries = scan_workspace(&roots);
                // The scan is stateless, so a fresh service suffices for extraction.
                let service = OtuiService::new();
                let mut indexed = 0usize;
                for (uri, text) in entries {
                    if shutdown.load(Ordering::Relaxed) {
                        return; // shutting down: stop promptly, drop the `Sender` clone
                    }
                    // Hold the reindex guard across the open-check AND the disk-index writes so a
                    // concurrent `did_open`/`did_change` cannot slip between them and be clobbered by
                    // stale disk text: an open buffer's index entry always wins (see `reindex_guard`).
                    let _guard = reindex_guard.lock().expect("reindex_guard poisoned");
                    // An open buffer is authoritative: if the file was opened while the scan ran, do
                    // not overwrite its buffer-derived index entry with disk text.
                    if documents
                        .read()
                        .expect("documents lock poisoned")
                        .contains_key(&uri)
                    {
                        continue;
                    }
                    let defs = service.style_defs(&text);
                    // Incremental: take the write locks per file and release them immediately, so the
                    // scan never blocks request handlers for long.
                    style_index
                        .write()
                        .expect("style_index lock poisoned")
                        .set_document(DocId::from(uri.to_string()), defs);
                    disk_texts
                        .write()
                        .expect("disk_texts lock poisoned")
                        .insert(uri, text);
                    indexed += 1;
                }
                // Then scan `.lua` for widget definitions (custom style props + `extends` parents)
                // AND id cross-references (spec §2.3: `getChildById`/.../`.ui.` — the other half of
                // the OTUI↔Lua bridge). No reindex guard, open-check, or disk-text cache for the
                // widget-def half: Lua is never an open OTUI document, so `lua_index` is fed purely
                // from disk. Files that declare no widget contribute an empty result there and are
                // skipped to keep that index lean.
                //
                // The two halves are indexed INDEPENDENTLY — a `.lua` file with refs but no widget
                // defs (the common case: most controllers never define a custom widget type) must
                // still land in `lua_ref_index`, so this is deliberately never a single `if
                // defs.is_empty() { continue }` gate shared between them.
                let mut lua_indexed = 0usize;
                let mut lua_refs_indexed = 0usize;
                for (uri, text) in scan_workspace_lua(&roots) {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    let defs = service.lua_widgets(&text);
                    if !defs.is_empty() {
                        lua_index
                            .write()
                            .expect("lua_index lock poisoned")
                            .set_document(DocId::from(uri.to_string()), defs);
                        lua_indexed += 1;
                    }
                    // Unlike the widget-def half above, `lua_ref_index` IS open-buffer aware (see
                    // `Backend::reindex_lua_refs_open`), so this write races against a concurrent
                    // `didOpen`/`didChange` for the very same `.lua` file exactly like the `.otui`
                    // loop above — and is guarded the same way: hold `reindex_guard` across the
                    // open-check and the write, so an open buffer's fresher ref index can never be
                    // clobbered by stale disk text found mid-scan.
                    let refs = scan_id_refs(&text);
                    if !refs.is_empty() {
                        let _guard = reindex_guard.lock().expect("reindex_guard poisoned");
                        if documents
                            .read()
                            .expect("documents lock poisoned")
                            .contains_key(&uri)
                        {
                            continue;
                        }
                        lua_ref_index
                            .write()
                            .expect("lua_ref_index lock poisoned")
                            .set_document(DocId::from(uri.to_string()), refs);
                        lua_texts
                            .write()
                            .expect("lua_texts lock poisoned")
                            .insert(uri, text);
                        lua_refs_indexed += 1;
                    }
                }
                // Then build the module-association index (`.otmod` → controllers → loaded `.otui`
                // files, module doc comment on `Backend::module_ui_index`). Independent of the two
                // scans above — it re-reads every module's `.otmod`/controllers/UI files itself
                // (`scan_module_dir`) rather than reusing `disk_texts`/`lua_texts`, so it is
                // unaffected by whichever of the loops above the shutdown flag cut short.
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                let built = build_module_index(&roots);
                let module_indexed = built.module_count();
                *module_ui_index
                    .write()
                    .expect("module_ui_index lock poisoned") = built;

                // Progress on stderr, never the LSP channel.
                eprintln!(
                    "otui-lsp: indexed {indexed} workspace .otui file(s), \
                     {lua_indexed} .lua widget file(s), {lua_refs_indexed} .lua ref file(s), \
                     {module_indexed} module(s) with a resolvable UI association"
                );
                // Completion refresh: the indexes are now complete, so re-diagnose every open
                // document to clear any stale widget-aware hint computed against a partial index
                // while the scan ran. Skipped if we are already shutting down (we are quitting).
                // Snapshot open buffers first so no document lock is held across the per-doc publish.
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                // (The OTUI-only language guard.) A `.lua` buffer never gets diagnostics from this
                // server — the analyzer must never run over Lua text — so it is excluded from the
                // refresh here too, not just in `Backend::publish`.
                let open: Vec<(Uri, String, i32)> = documents
                    .read()
                    .expect("documents lock poisoned")
                    .iter()
                    .filter(|(_, doc)| doc.language == Language::Otui)
                    .map(|(uri, doc)| (uri.clone(), doc.text.clone(), doc.version))
                    .collect();
                if !open.is_empty() {
                    let styles = style_index.read().expect("style_index lock poisoned");
                    let lua = lua_index.read().expect("lua_index lock poisoned");
                    // Same asset resolution as `Backend::publish`: the workspace roots computed once
                    // for the whole refresh batch, each document's own directory computed per-URI.
                    let workspace_roots: Vec<PathBuf> =
                        roots.iter().filter_map(uri_to_file_path).collect();
                    for (uri, text, version) in open {
                        let doc_dir =
                            uri_to_file_path(&uri).and_then(|p| p.parent().map(Path::to_path_buf));
                        compute_and_send_diagnostics(
                            &sender,
                            &service,
                            &styles,
                            &lua,
                            &documents,
                            encoding,
                            uri,
                            &text,
                            version,
                            doc_dir.as_deref(),
                            &workspace_roots,
                            &otpkg_cache,
                        );
                    }
                }
                // The scan just populated the style/Lua indexes from scratch — every code lens or
                // inlay hint an open document computed before this point (e.g. against an empty or
                // partial index) may now be stale, not only that document's diagnostics above. Skip
                // the request entirely if the scan found nothing to index (nothing could have
                // changed a lens/hint's answer).
                if indexed > 0 || lua_indexed > 0 {
                    send_workspace_refresh(
                        &sender,
                        code_lens_refresh_support,
                        inlay_hint_refresh_support,
                    );
                }
            });
        }

        self.log(MessageType::INFO, "otui-lsp server ready");
    }

    /// Send a fire-and-forget `client/registerCapability` request for `registration`. The client's
    /// ack arrives as a `Message::Response` the main loop ignores; we do not track it.
    fn register_capability(&self, request_id: &str, registration: Registration) {
        let params = RegistrationParams {
            registrations: vec![registration],
        };
        let request = Request::new(
            RequestId::from(request_id.to_owned()),
            "client/registerCapability".to_owned(),
            params,
        );
        let _ = self.sender.send(Message::Request(request));
    }

    fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            let uri = change.uri;

            // Module association bookkeeping runs for EVERY relevant change (`.otmod`/`.lua`/
            // `.otui`), independent of the open-buffer / style-index / Lua-index handling below —
            // it is pure disk-existence bookkeeping (see `update_module_index_for`'s doc comment),
            // so it must run before any branch below that `continue`s past it (the open-buffer
            // check, in particular: a currently-open `.otui`/`.lua` file's on-disk existence still
            // matters here even though its style/Lua-index re-indexing is skipped for it).
            self.update_module_index_for(&uri);

            if is_otmod_uri(&uri) {
                // A `.otmod` module manifest is never a style/Lua document in its own right —
                // `update_module_index_for` above already consumed it; routing it into either branch
                // below would feed OTML module-manifest text into `style_defs`/`lua_widgets` as if it
                // were a real `.otui`/`.lua` document.
                continue;
            }
            // A `.lua` widget module feeds the Lua index only (no open-buffer or disk-text tracking —
            // Lua is never an open OTUI document), so route it before the `.otui` logic.
            //
            // Classified through `Language::from_uri`, never by testing the string here: a
            // case-sensitive `ends_with(".lua")` sent `Mod.LUA` down the OTUI branch instead, into
            // `index_from_disk` → `style_defs` over Lua text → garbage in the workspace `StyleIndex`
            // and Lua text cached in `disk_texts` — from where a rename could rewrite bytes inside
            // that file. One rule, one place.
            if Language::from_uri(&uri) == Language::Lua {
                self.apply_lua_watch_change(&uri, change.typ);
                continue;
            }
            // Open buffer wins: a document open in the editor is authoritative over its on-disk copy,
            // so a disk event for it must never clobber the buffer-derived index entry. did_close
            // re-syncs it from disk once the buffer goes away.
            if self.is_open(&uri) {
                continue;
            }
            if change.typ == FileChangeType::DELETED {
                self.deindex(&uri);
            } else if change.typ == FileChangeType::CREATED || change.typ == FileChangeType::CHANGED
            {
                // Re-read from disk (synchronously — the single-threaded loop makes an inline read
                // fine) and re-index; skip (with a log) an unreadable/oversized/binary file.
                //
                // The old post-await open-state re-check is gone: with a single-threaded loop this
                // handler runs to completion before the next message is read, so no concurrent
                // `did_open` can slip in between the read and the index write. The race is impossible.
                if let Some(text) = read_indexed_file(&uri) {
                    self.index_from_disk(&uri, text);
                } else {
                    self.log(
                        MessageType::INFO,
                        format!("otui-lsp: skipped unreadable watched file {}", uri.as_str()),
                    );
                }
            }
        }
        // A watched change mutated the style and/or Lua index; refresh open buffers so their
        // widget-aware diagnostics reflect it instead of going stale until the next edit.
        self.republish_open_documents();
        // The same index mutation can also stale a code lens/inlay hint in an open document OTHER
        // than the one that was just watched (e.g. a subtype added in file B leaves file A's "N
        // widgets inherit this style" lens under-counting) — ask the client to re-request them.
        send_workspace_refresh(
            &self.sender,
            self.code_lens_refresh_support(),
            self.inlay_hint_refresh_support(),
        );
    }

    /// Apply one watched-file change for a `.lua` module to both [`lua_index`](Self::lua_index)
    /// (widget definitions) and [`lua_ref_index`](Self::lua_ref_index) (id cross-references, the
    /// other half of the OTUI↔Lua bridge). An unreadable/oversized/binary file is skipped (logged
    /// once, not twice).
    ///
    /// The two indexes are NOT treated the same here, mirroring how they already differ elsewhere:
    /// [`lua_index`](Self::lua_index) is "fed only from disk" by design (see
    /// [`index_lua_from_disk`](Self::index_lua_from_disk) — a `.lua` document's widget defs never
    /// reflect an open buffer's unsaved edits), so a watch event always re-scans it from disk, open
    /// or not. [`lua_ref_index`](Self::lua_ref_index)/[`lua_texts`](Self::lua_texts), in contrast,
    /// DO track the open buffer (`set_open_document` → `reindex_lua_refs_open`) — the same "open
    /// buffer wins" rule the OTUI branch of `did_change_watched_files` applies via `is_open` — so a
    /// disk event for a currently open `.lua` file must skip touching them, on both change and
    /// delete: reindexing from stale disk text would leave `lua_ref_index`'s spans mismatched with
    /// [`lua_text_for`](Self::lua_text_for)'s buffer text, and deindexing on delete would erase the
    /// open buffer's own entry out from under it. `did_close` re-syncs from disk once the buffer
    /// goes away.
    fn apply_lua_watch_change(&self, uri: &Uri, typ: FileChangeType) {
        let open = self.is_open(uri);
        if typ == FileChangeType::DELETED {
            self.deindex_lua(uri);
            if !open {
                self.deindex_lua_refs(uri);
            }
        } else if let Some(text) = read_indexed_file(uri) {
            self.index_lua_from_disk(uri, &text);
            if !open {
                self.index_lua_refs_from_disk(uri, text);
            }
        } else {
            self.log(
                MessageType::INFO,
                format!(
                    "otui-lsp: skipped unreadable watched lua file {}",
                    uri.as_str()
                ),
            );
        }
    }

    fn semantic_tokens_full(&self, params: SemanticTokensParams) -> Option<SemanticTokensResult> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; nothing to highlight for an unknown document, and
        // (the OTUI-only language guard) nothing for a `.lua` one either — semantic tokens for Lua
        // belong to the user's `lua-language-server`.
        let text = self.otui_document_text(&uri)?;

        let core_tokens = self.service.semantic_tokens(&text);
        let data = semantic::encode(&text, &core_tokens, self.encoding());

        Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        }))
    }

    fn document_symbol(&self, params: DocumentSymbolParams) -> Option<DocumentSymbolResponse> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; an unknown document has no outline, and (the
        // OTUI-only language guard) neither does a `.lua` one — `lua-language-server` owns that.
        let text = self.otui_document_text(&uri)?;

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
        Some(response)
    }

    fn document_color(&self, params: DocumentColorParams) -> Vec<ColorInformation> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; an unknown document has no colors, and (the
        // OTUI-only language guard) neither does a `.lua` one — `lua-language-server` owns that.
        // The request returns a plain `Vec` (not `Option`), so either case is the empty vec.
        let Some(text) = self.otui_document_text(&uri) else {
            return Vec::new();
        };

        let core_colors = self.service.document_colors(&text);
        convert::colors_to_lsp(&text, &core_colors, self.encoding())
    }

    /// `textDocument/documentLink`: make asset paths (`image-source: <path>`, `icon` family)
    /// clickable. Best-effort — a link is emitted only when the resolved target file actually exists
    /// on disk, so there are no dead links.
    fn document_link(&self, params: DocumentLinkParams) -> Option<Vec<DocumentLink>> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; an unknown document has no links, and (the
        // OTUI-only language guard) neither does a `.lua` one.
        let text = self.otui_document_text(&uri)?;

        // Only `file://` documents have a directory to resolve relative paths against.
        let doc_path = uri_to_file_path(&uri)?;
        let doc_dir = doc_path.parent()?.to_path_buf();
        let data_roots = self.asset_data_roots(&doc_dir);

        let encoding = self.encoding();
        let index = LineIndex::new(&text);
        let mut links = Vec::new();
        for PathRef { span, path, .. } in self.service.document_links(&text) {
            // Pure resolution → candidate filesystem paths; the `.is_file()` I/O is the only fs work,
            // kept thin here (a handful of links per document). `is_file()` (not `exists()`) so a path
            // resolving to a directory is not linked — a directory target isn't openable and would be
            // the very dead link this feature avoids.
            let Some(target_path) = resolve_asset_candidates(&path, &doc_dir, &data_roots)
                .into_iter()
                .find(|candidate| candidate.is_file())
            else {
                // No candidate resolves to an existing file → skip (no dead link).
                continue;
            };
            let Some(target) = uri_from_file_path(&target_path) else {
                continue;
            };
            links.push(DocumentLink {
                range: index.range(span.start, span.end, encoding),
                target: Some(target),
                tooltip: Some(format!("Open {path}")),
                data: None,
            });
        }
        Some(links)
    }

    /// `textDocument/codeLens`: "N widget(s) inherit this style" on a top-level style's declared
    /// name, shown only when at least one style derives from it directly (spec §5.2's
    /// `Name < Base` namespace is global, so a style's derivations can live in other documents).
    ///
    /// Eager, not lazy: `resolve_provider` is advertised `false`, so every `CodeLens` carries its
    /// final `Command` up front.
    ///
    /// There is no portable, editor-agnostic LSP command for "show these locations" — VS Code's
    /// built-in `editor.action.showReferences(uri, position, locations)` was considered and
    /// rejected, not overlooked: `Command.arguments` are forwarded to the client verbatim, as the
    /// raw LSP-JSON this handler serializes (`vscode-languageclient`'s `asCommand` copies
    /// `item.arguments` through unconverted), but `editor.action.showReferences` expects real
    /// `vscode.Uri`/`vscode.Position`/`vscode.Location` *class instances*, not their JSON shapes.
    /// Every existing caller of it (e.g. rust-analyzer) gets there by registering an
    /// extension-side command that does that JSON→API conversion before forwarding to the built-in
    /// one.
    ///
    /// So this server follows that same pattern: it emits its own namespaced command id,
    /// `otui.showSubtypes`, with plain-JSON `arguments: [uri, position]` (the style declaration's
    /// document and the lens's position). The companion VS Code extension registers
    /// `otui.showSubtypes` and, on click, re-runs `textDocument/implementation` at that position
    /// (which this server already answers from `subtypes`) and peeks the results — so the N
    /// derivations are never collected server-side here, only recomputed on demand by the
    /// extension. A client that has not registered the command renders the lens title and no-ops
    /// on click: harmless, same as an inert lens, but forward-compatible once the extension half
    /// ships.
    fn code_lens(&self, params: CodeLensParams) -> Option<Vec<CodeLens>> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; an unknown document has no lenses, and (the
        // OTUI-only language guard) neither does a `.lua` one.
        let text = self.otui_document_text(&uri)?;

        let encoding = self.encoding();
        let index = LineIndex::new(&text);
        let styles = self.style_index.read().expect("style_index lock poisoned");
        let lenses = style_lenses(&text, &styles)
            .into_iter()
            .map(|lens| {
                let n = lens.derived_count;
                let title = if n == 1 {
                    "1 widget inherits this style".to_owned()
                } else {
                    format!("{n} widgets inherit this style")
                };
                let range = index.range(lens.name_span.start, lens.name_span.end, encoding);
                CodeLens {
                    range,
                    command: Some(Command {
                        title,
                        command: "otui.showSubtypes".to_owned(),
                        arguments: Some(vec![
                            serde_json::to_value(&uri).expect("Uri serializes"),
                            serde_json::to_value(range.start).expect("Position serializes"),
                        ]),
                    }),
                    data: None,
                }
            })
            .collect();
        Some(lenses)
    }

    /// `textDocument/inlayHint`: the resolved native `UI*` ancestor after a based widget/style's
    /// `Base` token (`Foo < SomeStyle →UIButton`), so the reader sees what `Foo` ultimately *is*
    /// without hand-walking the `Name < Base` chain. Filtered to the requested `params.range`
    /// (the client's visible viewport), so a large document is never asked to hint off-screen.
    fn inlay_hint(&self, params: InlayHintParams) -> Option<Vec<InlayHint>> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; an unknown document has no hints, and (the
        // OTUI-only language guard) neither does a `.lua` one.
        let text = self.otui_document_text(&uri)?;

        let encoding = self.encoding();
        let index = LineIndex::new(&text);
        // Convert the viewport once to a byte range, so filtering each hint is a plain offset
        // comparison rather than a per-hint Position conversion.
        let range_start = index.offset_at(params.range.start, encoding);
        let range_end = index.offset_at(params.range.end, encoding);

        let hints = {
            let styles = self.style_index.read().expect("style_index lock poisoned");
            let lua = self.lua_index.read().expect("lua_index lock poisoned");
            ancestor_hints(&text, &styles, &lua)
        };

        let out = hints
            .into_iter()
            // LSP ranges are end-exclusive: a hint anchored exactly at `range_end` lies just past
            // the requested viewport, not inside it.
            .filter(|hint| range_start <= hint.anchor && hint.anchor < range_end)
            .map(|hint| InlayHint {
                position: index.position(hint.anchor, encoding),
                label: InlayHintLabel::from(format!("→ {}", hint.native)),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: None,
                data: None,
            })
            .collect();
        Some(out)
    }

    fn color_presentation(&self, params: ColorPresentationParams) -> Vec<ColorPresentation> {
        // (The OTUI-only language guard.) This handler never reads the document's text — it just
        // renders `params.color` back as text — but a `.lua` buffer must still get no color
        // presentations from us: `lua-language-server` owns that surface.
        if !self.is_otui_document(&params.text_document.uri) {
            return Vec::new();
        }
        // The picked color, as engine `Rgba`. `range` is where the new text is inserted (the token
        // being replaced) — so each presentation carries a `TextEdit` over that range.
        let color = params.color;
        let rgba = otui_core::schema::Rgba {
            r: color.red,
            g: color.green,
            b: color.blue,
            a: color.alpha,
        };
        otui_core::schema::color_presentations(rgba)
            .into_iter()
            .map(|label| ColorPresentation {
                text_edit: Some(TextEdit {
                    range: params.range,
                    new_text: label.clone(),
                }),
                label,
                additional_text_edits: None,
            })
            .collect()
    }

    fn folding_range(&self, params: FoldingRangeParams) -> Option<Vec<FoldingRange>> {
        let uri = params.text_document.uri;
        // Serve from the stored document text; an unknown document has nothing to fold, and (the
        // OTUI-only language guard) neither does a `.lua` one.
        let text = self.otui_document_text(&uri)?;

        let folds = self.service.folding_ranges(&text);
        Some(convert::folds_to_lsp(&folds))
    }

    fn goto_definition(&self, params: GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to resolve; the OTUI-only
        // language guard applies the same "nothing" to a `.lua` document — go-to-definition for Lua
        // is a later node, and belongs to `lua-language-server` until then).
        let text = self.otui_document_text(&uri)?;

        // Map the cursor Position to a byte offset, then classify the token under it.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let base_ref = self.service.base_reference_at(&text, offset)?;

        // Resolve against the workspace index, building each target range from its own document.
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
        resolve_base_definition(&index, &documents, &base_ref.name, encoding)
    }

    fn goto_type_definition(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> Option<GotoTypeDefinitionResponse> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to resolve; the OTUI-only
        // language guard applies the same "nothing" to a `.lua` document).
        let text = self.otui_document_text(&uri)?;

        // Classify the symbol under the cursor into the style name it is an instance of / declares.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let type_ref = self.service.style_type_at(&text, offset)?;

        // Resolve its declaration(s) from the cached workspace index (the namespace is global). A
        // native `UI*` type has no user declaration and so resolves to nothing.
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
        resolve_type_definition(&index, &documents, &type_ref.name, encoding)
    }

    fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Option<GotoImplementationResponse> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to resolve; the OTUI-only
        // language guard applies the same "nothing" to a `.lua` document).
        let text = self.otui_document_text(&uri)?;

        // Classify the style name under the cursor (a header name/base, or a widget instance treated
        // as its type); implementation lists who derives from that name.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let type_ref = self.service.style_type_at(&text, offset)?;

        // Aggregate the derivations from the cached workspace index (the namespace is global). No
        // user derivations → `None` (mirroring go-to-definition's empty-is-None convention).
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
        let locations = collect_implementations(&index, &documents, &type_ref.name, encoding);
        if locations.is_empty() {
            return None;
        }
        Some(GotoImplementationResponse::Array(locations))
    }

    fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to root on; the OTUI-only
        // language guard applies the same "nothing" to a `.lua` document).
        let text = self.otui_document_text(&uri)?;

        // Classify the symbol under the cursor into the style name it is an instance of / declares.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let type_ref = self.service.style_type_at(&text, offset)?;

        // Root the hierarchy on that style's declaration in the cached workspace index. A native
        // `UI*` name (or any name with no user declaration) has nothing to root on → `None`.
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
        prepare_type_hierarchy_item(&index, &documents, &type_ref.name, encoding)
            .map(|item| vec![item])
    }

    fn supertypes(&self, params: TypeHierarchySupertypesParams) -> Option<Vec<TypeHierarchyItem>> {
        let encoding = self.encoding();
        // The style name travels in the item's `data` (falling back to its `name`); the direct
        // supertype is its base, resolved fresh from the cached index (the namespace is global).
        let name = item_style_name(&params.item);
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
        // An empty list is the LSP "no supertypes" answer (a native/absent base ends the chain).
        Some(resolve_supertypes(&index, &documents, &name, encoding))
    }

    fn subtypes(&self, params: TypeHierarchySubtypesParams) -> Option<Vec<TypeHierarchyItem>> {
        let encoding = self.encoding();
        // The style name travels in the item's `data` (falling back to its `name`); the direct
        // subtypes are the styles deriving from it, read from the cached workspace index.
        let name = item_style_name(&params.item);
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
        // An empty list is a valid answer (nothing derives from this style).
        Some(resolve_subtypes(&index, &documents, &name, encoding))
    }

    /// `textDocument/references`, in **both** directions of the OTUI↔Lua bridge (spec §2.3):
    ///
    /// * Cursor on an OTUI `id:` value → the usual document-local OTUI references, PLUS every use of
    ///   that id in the **paired** `.lua` controller ([`lua_forward_references`](Self::lua_forward_references)).
    /// * Cursor on a `getChildById`/`recursiveGetChildById`/`.ui.` token in a `.lua` document → the
    ///   `id:` declaration site(s) in the paired `.otui` (and any inherited style body that declares
    ///   it) — [`lua_references`](Self::lua_references).
    ///
    /// A style name's references are unaffected either way (they never cross into Lua).
    fn references(&self, params: ReferenceParams) -> Option<Vec<Location>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let encoding = self.encoding();

        // Reverse direction: the request document is Lua, not OTUI — branch here rather than
        // through `otui_document_text` (which is `None` for a `.lua` document by construction; see
        // its doc comment).
        if self.document_language(&uri) == Language::Lua {
            return self.lua_references(&uri, position, encoding);
        }

        // Read the request document's text (unknown document → nothing to resolve).
        let text = self.otui_document_text(&uri)?;

        // Map the cursor Position to a byte offset, then classify what it is on. A cursor on neither
        // a style name nor an id has no references.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let target = classify_reference_target(&self.service, &text, offset)?;

        // Aggregate: style names fan out across the workspace; ids stay in the current document.
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
        let mut locations = collect_references(
            &target,
            &uri,
            &documents,
            &index,
            &self.service,
            include_declaration,
            encoding,
        );

        // Forward direction: append the id's uses in the paired `.lua` controller, scoped to that
        // single file — never the workspace-wide `LuaRefIndex::lookup` (see `paired_uri`'s doc
        // comment on why fanning out across every `.lua` mentioning the same id string would be
        // noise, not signal).
        if let ReferenceTarget::Id(id) = &target {
            locations.extend(self.lua_forward_references(&uri, id, encoding));
        }
        Some(locations)
    }

    /// The forward half of the OTUI↔Lua bridge: every use of `id` in every `.lua` controller
    /// associated with `otui_uri` ([`Backend::associated_uris`] — the same-stem fast path
    /// ([`paired_uri`]) plus the `.otmod`-derived module association), as LSP [`Location`]s in each
    /// file.
    ///
    /// Scoped by [`LuaRefIndex::document`] per associated file — never the unscoped
    /// [`LuaRefIndex::lookup`] (see [`lua_ref_index`](Self::lua_ref_index)'s doc comment: an id like
    /// `closeButton` repeats across dozens of unrelated modules in a real workspace). Every ref kind
    /// (`GetChildById`, `RecursiveGetChildById`, `DotUi`) is included on the same terms: each
    /// candidate is already filtered to `ref.id == id`, so a `DotUi` hit is, by construction, "a
    /// chain segment whose text equals this id" — the best-effort rule
    /// [`otui_core::lua_refs`] documents for that ambiguous form; nothing widens it further (e.g. no
    /// attempt to also fall back to a chain PREFIX or a differently-cased match). Empty when
    /// `otui_uri` has no associated file at all, or none of them have on-disk/open text, are
    /// indexed, or contribute a matching ref.
    fn lua_forward_references(
        &self,
        otui_uri: &Uri,
        id: &str,
        encoding: PositionEncoding,
    ) -> Vec<Location> {
        let mut out = Vec::new();
        for lua_uri in self.associated_uris(otui_uri, "lua") {
            let Some(lua_text) = self.lua_text_for(&lua_uri) else {
                continue;
            };
            let doc_id = DocId::from(lua_uri.to_string());
            let index = self
                .lua_ref_index
                .read()
                .expect("lua_ref_index lock poisoned");
            let Some(doc_refs) = index.document(&doc_id) else {
                continue;
            };
            out.extend(
                doc_refs
                    .iter()
                    .filter(|r| r.id == id)
                    .map(|r| convert::location_of(lua_uri.clone(), &lua_text, r.span, encoding)),
            );
        }
        out
    }

    /// The reverse half of the OTUI↔Lua bridge: the cursor sits in `lua_uri` on a
    /// `getChildById`/`recursiveGetChildById`/`.ui.` token — resolve it back to:
    ///
    /// 1. The `id:` declaration site(s) in every `.otui` file associated with `lua_uri`
    ///    ([`Backend::associated_uris`]; spec §2.3), including any inherited style body that
    ///    declares it ([`visible_ids`]).
    /// 2. Every `setId("id")` call **in this same `.lua` document** ([`lua_refs::scan_id_defs`]) —
    ///    the id's real, runtime declaration site: a widget created and id'd purely in Lua has no
    ///    `.otui id:` at all, so without this a `getChildById('x')` immediately preceded by that
    ///    same file's own `x:setId('x')` would resolve to nothing. Scanned on demand directly from
    ///    the text already in hand here (the open buffer or [`lua_texts`](Self::lua_texts)'s cached
    ///    disk read) — no persistent index, no extra freshness plumbing, since this document's text
    ///    is never stale relative to itself.
    ///
    /// Both candidate sources are offered TOGETHER, never one suppressing the other: an id can, in
    /// principle, be both declared in an `.otui` file and `setId`'d at runtime (e.g. a
    /// `.otui`-declared placeholder whose id is reassigned in code), and go-to-definition/references
    /// is navigation, not a uniqueness claim — surfacing every candidate site is strictly more useful
    /// than picking one and hiding the rest.
    ///
    /// `None` when the cursor is not on any recognized reference form ([`lua_refs::ref_at`]) — "not
    /// applicable here", the same answer every other OTUI-only handler gives for an unrelated
    /// position. Once a ref IS found, this always returns `Some` — an empty vec is a legitimate
    /// answer ("no declaration found"), matching the forward direction's `Some(locations)` shape.
    ///
    /// [`visible_ids`] is sound only for "might this id exist" (see its doc comment's
    /// over-approximation warning) — that is exactly the right shape for offering navigation
    /// targets, never for suppressing them, so every match it returns becomes a candidate `Location`
    /// here without further filtering. A `setId` literal match, in contrast, is EXACT — the literal
    /// IS the declaration — so it carries zero false-positive risk of its own.
    fn lua_references(
        &self,
        lua_uri: &Uri,
        position: lsp_types::Position,
        encoding: PositionEncoding,
    ) -> Option<Vec<Location>> {
        let text = self.lua_text_for(lua_uri)?;
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let target_ref = otui_core::lua_refs::ref_at(&text, offset)?;

        let mut out = Vec::new();
        let documents = self.merged_documents();
        let styles = self.style_index.read().expect("style_index lock poisoned");
        let lua_widgets = self.lua_index.read().expect("lua_index lock poisoned");
        for otui_uri in self.associated_uris(lua_uri, "otui") {
            let Some(otui_doc) = documents.get(&otui_uri) else {
                continue;
            };
            let visible = visible_ids(&otui_doc.text, &styles, &lua_widgets);
            for v in visible.iter().filter(|v| v.id == target_ref.id) {
                let loc = match &v.origin {
                    IdOrigin::Document => Some(convert::location_of(
                        otui_uri.clone(),
                        &otui_doc.text,
                        v.span,
                        encoding,
                    )),
                    IdOrigin::InheritedStyle { doc, .. } => {
                        resolve_location(doc, v.span, &documents, encoding)
                    }
                };
                if let Some(loc) = loc {
                    out.push(loc);
                }
            }
        }
        // Additional candidates: this SAME `.lua` document's own `setId("id")` declarations (see
        // this method's doc comment) — added alongside, never instead of, the `.otui` matches above.
        out.extend(
            otui_core::lua_refs::scan_id_defs(&text)
                .into_iter()
                .filter(|d| d.id == target_ref.id)
                .map(|d| convert::location_of(lua_uri.clone(), &text, d.span, encoding)),
        );
        Some(out)
    }

    fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Option<Vec<DocumentHighlight>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to highlight; the OTUI-only
        // language guard applies the same "nothing" to a `.lua` document). Highlights are
        // document-local, so only this buffer's text is ever needed — no merged view, no index.
        let text = self.otui_document_text(&uri)?;

        // Map the cursor Position to a byte offset, then classify what it is on (the SAME classifier
        // references/rename use, so the three features agree on what a symbol is). A cursor on neither
        // a style name nor an id has nothing to highlight.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let target = classify_reference_target(&self.service, &text, offset)?;

        Some(collect_document_highlights(
            &target,
            &text,
            &self.service,
            encoding,
        ))
    }

    fn prepare_rename(&self, params: TextDocumentPositionParams) -> Option<PrepareRenameResponse> {
        let uri = params.text_document.uri;
        let position = params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → not renameable; the OTUI-only
        // language guard applies the same "not renameable" to a `.lua` document).
        let text = self.otui_document_text(&uri)?;

        // Map the cursor Position to a byte offset, then classify the token under it. A cursor on
        // neither a style name nor an id is not renameable here → `None`.
        let line_index = LineIndex::new(&text);
        let offset = line_index.offset_at(position, encoding);
        let (target, span) = classify_rename_target(&self.service, &text, offset)?;

        // A native `UI*` base has no user declaration to rename → not user-renameable, so report it
        // as unrenameable (`None`) rather than pre-selecting a token that a rename would refuse.
        if let ReferenceTarget::StyleName(name) = &target {
            let index = self.style_index.read().expect("style_index lock poisoned");
            if is_native_base(name) && index.lookup(name).is_empty() {
                return None;
            }
        }

        // Echo the exact name/id token range so the client pre-selects it for editing.
        let range = line_index.range(span.start, span.end, encoding);
        Some(PrepareRenameResponse::Range(range))
    }

    fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>, String> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to rename; the OTUI-only
        // language guard applies the same "nothing" to a `.lua` document).
        let Some(text) = self.otui_document_text(&uri) else {
            return Ok(None);
        };

        // Classify what the cursor is on; a cursor on neither a style name nor an id has nothing to
        // rename.
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let Some((target, _span)) = classify_rename_target(&self.service, &text, offset) else {
            return Ok(None);
        };

        // Build the edits: style names fan out across the workspace; ids stay document-local. An
        // invalid `new_name` surfaces as an `Err(message)` the dispatch arm maps to a JSON-RPC error
        // (never a broken edit).
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
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

    fn symbol(&self, params: WorkspaceSymbolParams) -> Option<Vec<SymbolInformation>> {
        let encoding = self.encoding();
        // Take both read locks (mirroring `goto_definition`'s discipline: never nest a write lock).
        let documents = self.merged_documents();
        let index = self.style_index.read().expect("style_index lock poisoned");
        let symbols = collect_workspace_symbols(&index, &documents, &params.query, encoding);
        // Always return a list (empty is fine and conventional); never `None` for "no matches".
        Some(symbols)
    }

    fn hover(&self, params: HoverParams) -> Option<Hover> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let encoding = self.encoding();

        // Read the request document's text (unknown document → nothing to hover; the OTUI-only
        // language guard applies the same "nothing" to a `.lua` document).
        let text = self.otui_document_text(&uri)?;

        // Map the cursor Position to a byte offset, then let the engine describe the token under it,
        // resolving against the workspace index. Only the current doc's LineIndex is needed to map
        // the description's span back to a range.
        let line_index = LineIndex::new(&text);
        let offset = line_index.offset_at(position, encoding);
        let index = self.style_index.read().expect("style_index lock poisoned");
        if let Some(desc) = self.service.style_hover_at(&text, offset, &index) {
            return Some(render_hover(&desc, &line_index, encoding));
        }
        drop(index);
        // Not a style token — fall back to a property-key hover (value type from the catalog/schema
        // metadata; no workspace index needed).
        if let Some(pdesc) = self.service.property_hover_at(&text, offset) {
            return Some(render_property_hover(&pdesc, &line_index, encoding));
        }
        // Not a property key either — is the cursor on an asset path *value* (a sprite-preview
        // hover)? Same resolution as `document_link`/the missing-asset diagnostic; unlike the
        // diagnostic this is not a claim of absence, so it degrades silently (no image, no note) when
        // the path cannot be resolved rather than requiring a workspace root up front.
        let path_ref = self.service.asset_ref_at(&text, offset)?;
        let doc_dir = uri_to_file_path(&uri).and_then(|p| p.parent().map(Path::to_path_buf));
        let resolved = doc_dir.and_then(|doc_dir| {
            let data_roots = self.asset_data_roots(&doc_dir);
            resolve_asset_candidates(&path_ref.path, &doc_dir, &data_roots)
                .into_iter()
                .find(|candidate| candidate.is_file())
        });
        Some(render_asset_hover(
            &path_ref,
            resolved.as_deref(),
            &line_index,
            encoding,
        ))
    }

    fn completion(&self, params: CompletionParams) -> Option<CompletionResponse> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let encoding = self.encoding();

        // Serve from the stored document text; an unknown document has nothing to complete, and
        // (the OTUI-only language guard) neither does a `.lua` one.
        let text = self.otui_document_text(&uri)?;

        // Map the cursor Position to a byte offset, then ask the engine (widget-aware, so a `UITable`
        // offers its Lua-added `column-style` etc.) for the set that applies. An empty list is a
        // valid answer (no context here); return it as such rather than `None`, which some clients
        // treat as "retry".
        let offset = LineIndex::new(&text).offset_at(position, encoding);
        let items = {
            let styles = self.style_index.read().expect("style_index lock poisoned");
            let lua = self.lua_index.read().expect("lua_index lock poisoned");
            convert::completions_to_lsp(
                &self
                    .service
                    .complete_with_widgets(&text, offset, &styles, &lua),
                self.snippet_support(),
            )
        };
        Some(CompletionResponse::Array(items))
    }

    fn code_action(&self, params: CodeActionParams) -> Option<CodeActionResponse> {
        let uri = params.text_document.uri;
        let encoding = self.encoding();

        // Serve from the stored document text; an unknown document has nothing to fix, and (the
        // OTUI-only language guard) neither does a `.lua` one.
        let text = self.otui_document_text(&uri)?;

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
        Some(actions)
    }

    fn formatting(&self, params: DocumentFormattingParams) -> Option<Vec<TextEdit>> {
        let uri = params.text_document.uri;
        let encoding = self.encoding();

        // Serve from the stored document text; an unknown document has nothing to format, and
        // (the OTUI-only language guard) neither does a `.lua` one — `lua-language-server` owns
        // that surface.
        let text = self.otui_document_text(&uri)?;

        // Ask the engine to format. `None` means the document does not parse cleanly (parse error /
        // `ERROR`/`MISSING` node); per the safety gate we then return no edits. Otherwise reply with
        // a single whole-document replace of the formatted text.
        let formatted = self.service.format(&text)?;
        Some(vec![convert::full_document_edit(
            &text, formatted, encoding,
        )])
    }

    fn range_formatting(&self, params: DocumentRangeFormattingParams) -> Option<Vec<TextEdit>> {
        let uri = params.text_document.uri;
        let encoding = self.encoding();

        // Serve from the stored document text; an unknown document has nothing to format, and
        // (the OTUI-only language guard) neither does a `.lua` one.
        let text = self.otui_document_text(&uri)?;

        // LSP ranges are END-EXCLUSIVE, but `format_line_edits` takes an INCLUSIVE end line. A
        // selection that ends at `{ line: M, character: 0 }` (the shape editors produce when the
        // selection stops at the very start of line M — e.g. selecting through the end of line M-1)
        // does NOT include line M, so exclude it. `saturating_sub` keeps the end valid; if this makes
        // the inclusive end fall below the start, `format_line_edits` yields no edits.
        let inclusive_end_line = if params.range.end.character == 0 {
            params.range.end.line.saturating_sub(1)
        } else {
            params.range.end.line
        };

        // Format the whole document (the formatter needs the full CST for structural depth) and keep
        // only the edits for lines that intersect the requested range and actually changed. `None`
        // means the document does not parse cleanly (parse error / `ERROR`/`MISSING` node); per the
        // same safety gate as whole-document formatting we then return no edits. A range that only
        // partially covers a line still reformats that whole line — line granularity is the correct
        // unit for an indentation-structured language.
        let line_edits =
            self.service
                .format_line_edits(&text, params.range.start.line, inclusive_end_line)?;

        // Map each line edit onto a `TextEdit` whose range covers that whole original line, from
        // column 0 to the line's end (a huge column clamps to the line end, before any `\r\n`, via
        // `LineIndex::offset_at`). Replacing only the content leaves the line's terminator intact.
        let line_index = LineIndex::new(&text);
        let edits = line_edits
            .into_iter()
            .map(|edit| {
                let start = lsp_types::Position::new(edit.line, 0);
                let end_offset =
                    line_index.offset_at(lsp_types::Position::new(edit.line, u32::MAX), encoding);
                TextEdit {
                    range: lsp_types::Range {
                        start,
                        end: line_index.position(end_offset, encoding),
                    },
                    new_text: edit.new_text,
                }
            })
            .collect();
        Some(edits)
    }

    /// `textDocument/onTypeFormatting`: auto-indent the line Enter just created.
    ///
    /// This wraps [`OtuiService::indent_for_line`] — a **lexical**, CST-free computation that keeps
    /// working on a mid-edit/broken document (unlike [`Backend::formatting`]'s `format` /
    /// `format_line_edits`, which hard-gate on a clean parse and would refuse to act at the exact
    /// moment on-type formatting fires). That primitive only ever proposes the previous line's depth
    /// or one level deeper: it cannot, and must not, guess a dedent — returning to a shallower level
    /// is always a user action (Backspace / Shift+Tab). Consequently this handler only ever edits
    /// the single line `params.text_document_position.position.line` names (the line the newline
    /// just produced); it never touches any other line, however "wrong" that other line's existing
    /// indentation looks — reindenting an existing line could silently move it under a different
    /// parent and change what the UI does.
    fn on_type_formatting(&self, params: DocumentOnTypeFormattingParams) -> Option<Vec<TextEdit>> {
        // Only Enter is wired: there is no trigger character for a dedent (see the capability
        // registration in `initialize_result`), so any other typed character is a no-op here.
        if params.ch != "\n" {
            return None;
        }

        let uri = params.text_document_position.text_document.uri;
        let encoding = self.encoding();

        // Serve from the stored document text; an unknown (not open) document has nothing to
        // format, matching every other formatting handler — and (the OTUI-only language guard)
        // neither does a `.lua` one.
        let text = self.otui_document_text(&uri)?;

        let line = params.text_document_position.position.line;

        // `None` means "make no edit": inside a block-scalar body (that indentation is raw Lua
        // content — reindenting it would be data loss) or on a tab-indented line (the
        // `tab-indentation` diagnostic + quick fix owns that, not this handler). Never substitute a
        // guess for either case.
        let target = self.service.indent_for_line(&text, line)?;

        // The line's existing leading spaces, counted the same way the engine counts indentation
        // (`otui_core::indent::leading_spaces`: a run of ASCII spaces, stopping at the first
        // non-space byte).
        let line_index = LineIndex::new(&text);
        let line_start = line_index.offset_at(lsp_types::Position::new(line, 0), encoding);
        let current = text[line_start..]
            .bytes()
            .take_while(|&b| b == b' ')
            .count();

        // Idempotence: most clients already run their own auto-indent on Enter, so an already
        // correct line must produce no edit — echoing a no-op edit would just make the buffer churn.
        if current == target {
            return None;
        }

        let whitespace_end = line_start + current;
        Some(vec![TextEdit {
            range: lsp_types::Range {
                start: lsp_types::Position::new(line, 0),
                end: line_index.position(whitespace_end, encoding),
            },
            new_text: " ".repeat(target),
        }])
    }

    fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = doc.uri;
        let version = doc.version;
        // The one place a document's language is learned from the client. Both signals are consulted
        // (see `Language::classify`): a `.lua` URI is Lua even if the client labels it something
        // else, because the guard must not hinge on a `languageId` string chosen in another repo.
        let language = Language::classify(&uri, &doc.language_id);
        // Insert the buffer and (for an OTUI document) re-index atomically w.r.t. the background
        // scan (see `set_open_document`), so a scan in flight cannot clobber this open buffer's
        // index entry.
        self.set_open_document(&uri, &doc.text, version, language);
        self.publish(uri, &doc.text, version);
    }

    fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last content change carries the entire new document text.
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let text = change.text;
        // `didChange` carries no `languageId`, so keep whatever `didOpen` recorded for this URI
        // (falling back to the URI extension if, somehow, we never saw an open for it).
        let language = self.document_language(&uri);
        // Same atomic buffer-insert + re-index as `did_open` (see `set_open_document`).
        self.set_open_document(&uri, &text, version, language);
        self.publish(uri, &text, version);
    }

    fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let language = {
            let mut docs = self.documents.write().expect("documents lock poisoned");
            docs.remove(&uri)
                .map(|doc| doc.language)
                .unwrap_or_else(|| Language::from_uri(&uri))
        };
        // Clear diagnostics for the closed document either way — a client needs it to clear
        // anything stale, whatever the language.
        self.send_diagnostics(uri.clone(), Vec::new(), None);

        // Semantics change (workspace index): closing a file no longer drops it from the index.
        // Under open-only indexing that was correct; now a closed file still lives on disk and must
        // stay indexed as a closed file. Re-read it from disk (inline — the single-threaded loop
        // makes a sync read fine) and re-index from that text (the buffer's unsaved edits, if any,
        // are discarded on close — disk is now authoritative). If the disk read fails (the file was
        // deleted while open), drop it from the relevant index + cache instead.
        //
        // The old post-await open-state re-check is gone: with a single-threaded loop this handler
        // runs to completion before the next message is read, so no concurrent `did_open` can slip in
        // between the read and the index write. The race is impossible.
        //
        // (The OTUI-only language guard, narrowed.) A `.lua` document was never fed into the OTUI
        // `StyleIndex` on open (`set_open_document` skips it), so there is nothing to re-sync THERE —
        // but it WAS fed into `lua_ref_index` from its open-buffer text (the bridge's reverse
        // direction), and that index must not go stale (or keep referencing unsaved-then-discarded
        // spans) once the buffer closes, exactly like the OTUI branch below re-syncs `style_index`.
        match language {
            Language::Otui => match read_indexed_file(&uri) {
                Some(text) => self.index_from_disk(&uri, text),
                None => self.deindex(&uri),
            },
            Language::Lua => match read_indexed_file(&uri) {
                Some(text) => self.index_lua_refs_from_disk(&uri, text),
                None => self.deindex_lua_refs(&uri),
            },
        }
    }
}

/// The exit status the server process should terminate with, per the LSP lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// A clean `shutdown` request followed by `exit`: exit the process with status 0.
    Shutdown,
    /// An `exit` notification with no preceding `shutdown` (or the peer closing the connection):
    /// the LSP spec requires terminating with a non-zero status (1).
    Aborted,
}

impl Termination {
    /// The process exit code for this termination (`0` clean, `1` aborted).
    pub fn exit_code(self) -> i32 {
        match self {
            Termination::Shutdown => 0,
            Termination::Aborted => 1,
        }
    }
}

/// Run the server's blocking, single-threaded receive loop until the LSP lifecycle ends, returning
/// how the process should terminate. Shared by the `otui-lsp` binary and the transport test.
///
/// [`Connection::handle_shutdown`] answers a `shutdown` request and then blocks for the client's
/// paired `exit`, so the clean `shutdown` → `exit` handshake resolves there and yields
/// [`Termination::Shutdown`] (exit 0). A bare `exit` notification reaching the notification arm is
/// therefore a *standalone* exit (no prior `shutdown`); per the spec the server must terminate with
/// a non-zero status, so we stop and report [`Termination::Aborted`] (exit 1) instead of silently
/// dropping it in `handle_notification`. A closed receiver (peer hung up) is likewise abnormal.
pub fn serve(
    backend: &Backend,
    connection: &Connection,
) -> Result<Termination, Box<dyn std::error::Error + Sync + Send>> {
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    return Ok(Termination::Shutdown);
                }
                let response = backend.handle_request(request);
                connection.sender.send(Message::Response(response))?;
            }
            Message::Notification(note) => {
                // A standalone `exit` (the paired `shutdown` → `exit` is consumed by
                // `handle_shutdown` above) must terminate the server with a non-zero status.
                if note.method == "exit" {
                    return Ok(Termination::Aborted);
                }
                backend.handle_notification(note);
            }
            // A `Message::Response` is the client's reply to one of OUR server→client requests
            // (the `client/registerCapability` acks); we do not track them.
            Message::Response(_) => {}
        }
    }
    Ok(Termination::Aborted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        ClientCapabilities, CompletionClientCapabilities, CompletionItemCapability,
        DocumentSymbolClientCapabilities, GeneralClientCapabilities, InsertTextFormat, Position,
        Range, TextDocumentClientCapabilities,
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

    fn params_with_snippet_support(support: Option<bool>) -> InitializeParams {
        InitializeParams {
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    completion: Some(CompletionClientCapabilities {
                        completion_item: Some(CompletionItemCapability {
                            snippet_support: support,
                            ..CompletionItemCapability::default()
                        }),
                        ..CompletionClientCapabilities::default()
                    }),
                    ..TextDocumentClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        }
    }

    #[test]
    fn snippet_support_default_false_when_client_is_silent() {
        // No textDocument capabilities at all → the LSP default (no snippets) applies.
        assert!(!client_supports_snippets(&InitializeParams::default()));
        // completion/completionItem present but the flag omitted → still the default.
        assert!(!client_supports_snippets(&params_with_snippet_support(
            None
        )));
    }

    #[test]
    fn snippet_support_true_only_when_client_opts_in() {
        assert!(client_supports_snippets(&params_with_snippet_support(
            Some(true)
        )));
        assert!(!client_supports_snippets(&params_with_snippet_support(
            Some(false)
        )));
    }

    #[test]
    fn backend_new_reads_snippet_support_from_init_params() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &params_with_snippet_support(Some(true)));
        assert!(backend.snippet_support());

        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        assert!(!backend.snippet_support());
    }

    fn params_with_refresh_support(
        code_lens: Option<bool>,
        inlay_hint: Option<bool>,
    ) -> InitializeParams {
        use lsp_types::{
            CodeLensWorkspaceClientCapabilities, InlayHintWorkspaceClientCapabilities,
            WorkspaceClientCapabilities,
        };
        InitializeParams {
            capabilities: ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    code_lens: Some(CodeLensWorkspaceClientCapabilities {
                        refresh_support: code_lens,
                    }),
                    inlay_hint: Some(InlayHintWorkspaceClientCapabilities {
                        refresh_support: inlay_hint,
                    }),
                    ..WorkspaceClientCapabilities::default()
                }),
                ..ClientCapabilities::default()
            },
            ..InitializeParams::default()
        }
    }

    #[test]
    fn refresh_support_default_false_when_client_is_silent() {
        // No workspace capabilities at all → the LSP default (no refresh requests) applies.
        assert!(!client_supports_code_lens_refresh(
            &InitializeParams::default()
        ));
        assert!(!client_supports_inlay_hint_refresh(
            &InitializeParams::default()
        ));
        // The workspace/codeLens/inlayHint capability objects present but the flag omitted → still
        // the default.
        assert!(!client_supports_code_lens_refresh(
            &params_with_refresh_support(None, None)
        ));
        assert!(!client_supports_inlay_hint_refresh(
            &params_with_refresh_support(None, None)
        ));
    }

    #[test]
    fn refresh_support_true_only_when_client_opts_in_per_kind() {
        // Each kind is read independently — a client that opts into one but not the other must not
        // get both, or neither, conflated.
        assert!(client_supports_code_lens_refresh(
            &params_with_refresh_support(Some(true), Some(false))
        ));
        assert!(!client_supports_inlay_hint_refresh(
            &params_with_refresh_support(Some(true), Some(false))
        ));
        assert!(!client_supports_code_lens_refresh(
            &params_with_refresh_support(Some(false), Some(true))
        ));
        assert!(client_supports_inlay_hint_refresh(
            &params_with_refresh_support(Some(false), Some(true))
        ));
    }

    #[test]
    fn backend_new_reads_refresh_support_from_init_params() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &params_with_refresh_support(Some(true), Some(true)));
        assert!(backend.code_lens_refresh_support());
        assert!(backend.inlay_hint_refresh_support());

        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        assert!(!backend.code_lens_refresh_support());
        assert!(!backend.inlay_hint_refresh_support());
    }

    /// Drain pending messages on `rx` and report whether a `Message::Request` for `method` (a
    /// server→client `workspace/codeLens/refresh` or `workspace/inlayHint/refresh`) was among them.
    fn drain_has_refresh_request(rx: &crossbeam_channel::Receiver<Message>, method: &str) -> bool {
        let mut found = false;
        while let Ok(msg) = rx.try_recv() {
            if let Message::Request(req) = msg
                && req.method == method
            {
                found = true;
            }
        }
        found
    }

    #[test]
    fn watched_file_change_sends_refresh_requests_only_when_the_client_opted_in() {
        use lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        // A client that advertised both refresh capabilities gets both requests after a watched
        // change mutates the index.
        let base = std::env::temp_dir().join(format!("otui-refresh-watch-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("mkdir");
        let otui_path = base.join("styles.otui");
        std::fs::write(&otui_path, "Panel < UIWidget\n").expect("write otui");
        let otui_uri = uri_from_file_path(&otui_path).expect("uri");

        let (tx, rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &params_with_refresh_support(Some(true), Some(true)));
        backend.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: otui_uri.clone(),
                typ: FileChangeType::CREATED,
            }],
        });
        assert!(
            drain_has_refresh_request(&rx, "workspace/codeLens/refresh"),
            "an opted-in client must get a codeLens/refresh after a watched-file index mutation"
        );

        // Re-run the drain against a fresh receiver (the previous drain consumed the channel) to
        // check the inlayHint side independently.
        let (tx2, rx2) = crossbeam_channel::unbounded();
        let backend2 = Backend::new(tx2, &params_with_refresh_support(Some(true), Some(true)));
        backend2.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: otui_uri.clone(),
                typ: FileChangeType::CREATED,
            }],
        });
        assert!(
            drain_has_refresh_request(&rx2, "workspace/inlayHint/refresh"),
            "an opted-in client must get an inlayHint/refresh after a watched-file index mutation"
        );

        // A client that never advertised either capability gets neither request — only the ignored
        // (unrelated) diagnostics republish.
        let (tx3, rx3) = crossbeam_channel::unbounded();
        let backend3 = Backend::new(tx3, &InitializeParams::default());
        backend3.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: otui_uri,
                typ: FileChangeType::CREATED,
            }],
        });
        assert!(
            !drain_has_refresh_request(&rx3, "workspace/codeLens/refresh"),
            "a client that never opted in must not receive a codeLens/refresh request"
        );
        assert!(
            !drain_has_refresh_request(&rx3, "workspace/inlayHint/refresh"),
            "a client that never opted in must not receive an inlayHint/refresh request"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn initial_scan_completion_sends_refresh_requests_when_the_client_opted_in() {
        use std::time::Duration;

        let base = std::env::temp_dir().join(format!("otui-refresh-scan-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(base.join("styles.otui"), "Panel < UIWidget\n").expect("write otui");

        let (tx, rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &params_with_refresh_support(Some(true), Some(true)));
        *backend.roots.lock().expect("roots") = vec![uri_from_file_path(&base).expect("root")];

        backend.run_initialized();

        // Wait (bounded) for a `workspace/codeLens/refresh` request; the scan runs on a background
        // thread, so this is not necessarily the first message on the channel.
        let mut got_code_lens_refresh = false;
        let mut got_inlay_hint_refresh = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !(got_code_lens_refresh && got_inlay_hint_refresh)
        {
            let Ok(msg) = rx.recv_timeout(Duration::from_secs(5)) else {
                break;
            };
            if let Message::Request(req) = msg {
                match req.method.as_str() {
                    "workspace/codeLens/refresh" => got_code_lens_refresh = true,
                    "workspace/inlayHint/refresh" => got_inlay_hint_refresh = true,
                    _ => {}
                }
            }
        }
        backend.signal_shutdown();
        assert!(
            got_code_lens_refresh,
            "the initial scan's completion should send workspace/codeLens/refresh"
        );
        assert!(
            got_inlay_hint_refresh,
            "the initial scan's completion should send workspace/inlayHint/refresh"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    /// Build the `CompletionParams` for a cursor `position` in `uri`.
    fn completion_params(uri: &Uri, position: Position) -> CompletionParams {
        use lsp_types::{
            PartialResultParams, TextDocumentIdentifier, TextDocumentPositionParams,
            WorkDoneProgressParams,
        };
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        }
    }

    #[test]
    fn completion_handler_sends_snippets_only_when_the_client_negotiated_support() {
        // End-to-end through the real `textDocument/completion` handler: a property-key completion
        // carries the `key: $0` snippet when the client opted in, and is bare-label plain text
        // otherwise — the gate lives entirely in capability negotiation, not in the engine.
        let uri = Uri::from_str("file:///ws/win.otui").expect("uri");
        let text = "Button\n  wid\n";
        let position = Position::new(1, 5); // just past "wid"

        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &params_with_snippet_support(Some(true)));
        backend.documents.write().expect("documents").insert(
            uri.clone(),
            Document {
                text: text.to_owned(),
                version: 0,
                language: Language::Otui,
            },
        );
        let response = backend
            .completion(completion_params(&uri, position))
            .expect("completion response");
        let CompletionResponse::Array(items) = response else {
            panic!("expected an array response");
        };
        let width = items
            .iter()
            .find(|i| i.label == "width")
            .expect("width offered");
        assert_eq!(width.insert_text.as_deref(), Some("width: $0"));
        assert_eq!(width.insert_text_format, Some(InsertTextFormat::SNIPPET));

        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        backend.documents.write().expect("documents").insert(
            uri.clone(),
            Document {
                text: text.to_owned(),
                version: 0,
                language: Language::Otui,
            },
        );
        let response = backend
            .completion(completion_params(&uri, position))
            .expect("completion response");
        let CompletionResponse::Array(items) = response else {
            panic!("expected an array response");
        };
        let width = items
            .iter()
            .find(|i| i.label == "width")
            .expect("width offered");
        assert_eq!(width.insert_text, None);
        assert_eq!(width.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));
    }

    /// Build a `(StyleIndex, documents)` pair from `(uri, text)` entries, indexing each document's
    /// style defs exactly the way the backend does on open/change.
    fn workspace(entries: &[(&str, &str)]) -> (StyleIndex, HashMap<Uri, Document>) {
        let svc = OtuiService::new();
        let mut index = StyleIndex::new();
        let mut documents = HashMap::new();
        for (uri_str, text) in entries {
            let uri = Uri::from_str(uri_str).expect("valid uri");
            index.set_document(DocId::from(uri.to_string()), svc.style_defs(text));
            documents.insert(
                uri,
                Document {
                    text: (*text).to_owned(),
                    version: 1,
                    language: Language::Otui,
                },
            );
        }
        (index, documents)
    }

    /// Build a `StyleIndex` + `disk_texts` map from `(uri, text)` entries, indexing each the way the
    /// workspace scan / did_close does (as a *closed* file: index its defs, cache its disk text).
    fn disk_workspace(entries: &[(&str, &str)]) -> (StyleIndex, HashMap<Uri, String>) {
        let svc = OtuiService::new();
        let mut index = StyleIndex::new();
        let mut disk = HashMap::new();
        for (uri_str, text) in entries {
            let uri = Uri::from_str(uri_str).expect("valid uri");
            index.set_document(DocId::from(uri.to_string()), svc.style_defs(text));
            disk.insert(uri, (*text).to_owned());
        }
        (index, disk)
    }

    #[test]
    fn merge_prefers_the_open_buffer_over_a_stale_disk_entry() {
        // Same URI in both views: the open buffer (unsaved edits) must win over the on-disk copy.
        let uri = Uri::from_str("file:///a.otui").expect("uri");
        let mut open = HashMap::new();
        open.insert(
            uri.clone(),
            Document {
                text: "Buffer < UIWidget\n".to_owned(),
                version: 7,
                language: Language::Otui,
            },
        );
        let mut disk = HashMap::new();
        disk.insert(uri.clone(), "Disk < UIWidget\n".to_owned());

        let merged = merge_documents(&open, &disk);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[&uri].text, "Buffer < UIWidget\n");
        assert_eq!(merged[&uri].version, 7);
    }

    #[test]
    fn merge_resolves_a_closed_uri_to_its_disk_text() {
        // A URI present only on disk (never opened) resolves to the disk text.
        let open = HashMap::new();
        let disk_uri = Uri::from_str("file:///closed.otui").expect("uri");
        let mut disk = HashMap::new();
        disk.insert(disk_uri.clone(), "Closed < UIWidget\n".to_owned());

        let merged = merge_documents(&open, &disk);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[&disk_uri].text, "Closed < UIWidget\n");
    }

    #[test]
    fn merge_unions_open_and_disk_only_uris() {
        // One URI open, a different one only on disk: the merged view contains both.
        let open_uri = Uri::from_str("file:///open.otui").expect("uri");
        let disk_uri = Uri::from_str("file:///disk.otui").expect("uri");
        let mut open = HashMap::new();
        open.insert(
            open_uri.clone(),
            Document {
                text: "Open < UIWidget\n".to_owned(),
                version: 1,
                language: Language::Otui,
            },
        );
        let mut disk = HashMap::new();
        disk.insert(disk_uri.clone(), "Disk < UIWidget\n".to_owned());

        let merged = merge_documents(&open, &disk);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[&open_uri].text, "Open < UIWidget\n");
        assert_eq!(merged[&disk_uri].text, "Disk < UIWidget\n");
    }

    #[test]
    fn references_resolve_across_a_mix_of_open_and_disk_only_files() {
        // `MyPanel` is declared in a CLOSED (disk-only) file and used as a base in an OPEN one. With
        // the merged view, references must span both — the whole point of a workspace-wide index.
        let (index, disk) = disk_workspace(&[("file:///defs.otui", "MyPanel < UIWidget\n")]);
        let mut open = HashMap::new();
        let use_uri = Uri::from_str("file:///use.otui").expect("uri");
        open.insert(
            use_uri.clone(),
            Document {
                text: "Child < MyPanel\n".to_owned(),
                version: 1,
                language: Language::Otui,
            },
        );
        let documents = merge_documents(&open, &disk);

        let svc = OtuiService::new();
        let target = ReferenceTarget::StyleName("MyPanel".to_owned());
        let locs = collect_references(
            &target,
            &use_uri,
            &documents,
            &index,
            &svc,
            true,
            PositionEncoding::Utf16,
        );
        // The declaration site (closed defs.otui) and the base reference (open use.otui) both resolve.
        assert_eq!(
            sorted_locs(&locs),
            vec![
                (
                    "file:///defs.otui".to_owned(),
                    Position::new(0, 0),
                    Position::new(0, 7)
                ),
                (
                    "file:///use.otui".to_owned(),
                    Position::new(0, 8),
                    Position::new(0, 15)
                ),
            ]
        );
    }

    #[test]
    fn rename_rewrites_across_open_and_disk_only_files() {
        // Declaration on disk, base ref open: a workspace rename must edit both files.
        let (index, disk) = disk_workspace(&[("file:///defs.otui", "MyPanel < UIWidget\n")]);
        let mut open = HashMap::new();
        let use_uri = Uri::from_str("file:///use.otui").expect("uri");
        open.insert(
            use_uri.clone(),
            Document {
                text: "Child < MyPanel\n".to_owned(),
                version: 1,
                language: Language::Otui,
            },
        );
        let documents = merge_documents(&open, &disk);

        let svc = OtuiService::new();
        let target = ReferenceTarget::StyleName("MyPanel".to_owned());
        let edit = build_rename_edits(
            &target,
            &use_uri,
            &documents,
            &index,
            &svc,
            "Renamed",
            PositionEncoding::Utf16,
        )
        .expect("valid new name")
        .expect("some edits");
        let changes = edit.changes.expect("changes keyed by URI");
        assert_eq!(
            changes.len(),
            2,
            "both the closed def and the open use are edited"
        );
        assert!(changes.contains_key(&Uri::from_str("file:///defs.otui").expect("uri")));
        assert!(changes.contains_key(&use_uri));
    }

    #[test]
    fn open_buffer_wins_when_the_same_uri_is_also_on_disk() {
        // A stale disk entry for `Old` plus an open buffer redefining it as `New`. The merged view
        // must resolve the URI to the buffer, so definition lookup sees `New`, not `Old`.
        let (_stale_index, disk) = disk_workspace(&[("file:///a.otui", "Old < UIWidget\n")]);
        let uri = Uri::from_str("file:///a.otui").expect("uri");
        let mut open = HashMap::new();
        open.insert(
            uri.clone(),
            Document {
                text: "New < UIWidget\n".to_owned(),
                version: 2,
                language: Language::Otui,
            },
        );
        // The index reflects the open buffer (as did_open would have re-indexed it).
        let svc = OtuiService::new();
        let mut index = StyleIndex::new();
        index.set_document(
            DocId::from(uri.to_string()),
            svc.style_defs("New < UIWidget\n"),
        );

        let documents = merge_documents(&open, &disk);
        // `New` resolves (against the buffer text); the stale `Old` no longer exists anywhere.
        assert!(
            resolve_base_definition(&index, &documents, "New", PositionEncoding::Utf16).is_some()
        );
        assert!(
            resolve_base_definition(&index, &documents, "Old", PositionEncoding::Utf16).is_none()
        );
    }

    /// Build the `DidOpenTextDocumentParams` for `uri`/`text` with the given `languageId`.
    fn did_open_params(uri: &Uri, language_id: &str, text: &str) -> DidOpenTextDocumentParams {
        use lsp_types::TextDocumentItem;
        DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.to_owned(),
                version: 1,
                text: text.to_owned(),
            },
        }
    }

    #[test]
    fn opening_a_lua_document_publishes_empty_diagnostics_never_running_the_otui_analyzer() {
        // Real Lua the OTUI analyzer would flag hard if it (wrongly) ran over it: tab
        // indentation is a hard error (`tab-indentation`) per spec, and the shape isn't valid
        // OTUI either.
        let text = "local x = 1\n\tif x then\n\t\tprint('hi')\n\tend\n";
        // Sanity check on the fixture itself (not the guard): prove the OTUI analyzer really
        // would flag this text if it ever ran over it, so the assertion below is not vacuous.
        let sanity_diags = OtuiService::new().diagnostics(text);
        assert!(
            sanity_diags.iter().any(|d| d.code == "tab-indentation"),
            "fixture must trip the OTUI analyzer directly: {sanity_diags:?}"
        );

        let uri = Uri::from_str("file:///ws/widget.lua").expect("uri");
        let (tx, rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        backend.did_open(did_open_params(&uri, "lua", text));

        // A `publishDiagnostics` notification must still have gone out (a client needs it to
        // clear anything stale) — but with an empty diagnostics array, never the OTUI codes the
        // fixture would trip.
        let mut published = None;
        while let Ok(msg) = rx.try_recv() {
            if let Message::Notification(note) = msg
                && note.method == "textDocument/publishDiagnostics"
            {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(note.params).expect("diagnostics params");
                if params.uri == uri {
                    published = Some(params.diagnostics);
                }
            }
        }
        assert_eq!(
            published,
            Some(Vec::new()),
            "a .lua document must still get an (empty) publishDiagnostics"
        );
    }

    #[test]
    fn every_otui_only_handler_is_a_no_op_for_a_lua_document() {
        use lsp_types::{
            CodeActionContext, Color, ColorPresentationParams, FormattingOptions,
            PartialResultParams, Position, Range, ReferenceContext, TextDocumentIdentifier,
            TextDocumentPositionParams, WorkDoneProgressParams,
        };

        let uri = Uri::from_str("file:///ws/widget.lua").expect("uri");
        let text = "local x = 1\nfunction f() end\n";
        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        backend.did_open(did_open_params(&uri, "lua", text));

        let position = Position::new(0, 0);
        let text_document = TextDocumentIdentifier { uri: uri.clone() };
        let text_document_position = TextDocumentPositionParams {
            text_document: text_document.clone(),
            position,
        };
        let range = Range {
            start: position,
            end: position,
        };

        assert!(
            backend
                .semantic_tokens_full(SemanticTokensParams {
                    text_document: text_document.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .document_symbol(DocumentSymbolParams {
                    text_document: text_document.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .document_color(DocumentColorParams {
                    text_document: text_document.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_empty()
        );
        assert!(backend.document_link(link_params(&uri)).is_none());
        assert!(
            backend
                .color_presentation(ColorPresentationParams {
                    text_document: text_document.clone(),
                    color: Color {
                        red: 1.0,
                        green: 1.0,
                        blue: 1.0,
                        alpha: 1.0,
                    },
                    range,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_empty()
        );
        assert!(
            backend
                .folding_range(FoldingRangeParams {
                    text_document: text_document.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .goto_definition(GotoDefinitionParams {
                    text_document_position_params: text_document_position.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .goto_type_definition(GotoTypeDefinitionParams {
                    text_document_position_params: text_document_position.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .goto_implementation(GotoImplementationParams {
                    text_document_position_params: text_document_position.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .prepare_type_hierarchy(TypeHierarchyPrepareParams {
                    text_document_position_params: text_document_position.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .references(ReferenceParams {
                    text_document_position: text_document_position.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: ReferenceContext {
                        include_declaration: true,
                    },
                })
                .is_none()
        );
        assert!(
            backend
                .document_highlight(DocumentHighlightParams {
                    text_document_position_params: text_document_position.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .prepare_rename(text_document_position.clone())
                .is_none()
        );
        assert_eq!(
            backend.rename(RenameParams {
                text_document_position: text_document_position.clone(),
                new_name: "Renamed".to_owned(),
                work_done_progress_params: WorkDoneProgressParams::default(),
            }),
            Ok(None)
        );
        assert!(
            backend
                .hover(HoverParams {
                    text_document_position_params: text_document_position.clone(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .completion(completion_params(&uri, position))
                .is_none()
        );
        assert!(
            backend
                .code_action(CodeActionParams {
                    text_document: text_document.clone(),
                    range,
                    context: CodeActionContext::default(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .formatting(DocumentFormattingParams {
                    text_document: text_document.clone(),
                    options: FormattingOptions::default(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .is_none()
        );
        assert!(
            backend
                .range_formatting(range_formatting_params(&uri, range))
                .is_none()
        );
        assert!(
            backend
                .on_type_formatting(on_type_formatting_params(&uri, position, "\n"))
                .is_none()
        );
    }

    #[test]
    fn an_otui_document_is_unaffected_by_the_language_guard() {
        // The `didOpen`-carried `languageId` for an ordinary `.otui` document must not change
        // any existing behavior: diagnostics still get computed and published, and a handler
        // (hover, here) still answers normally.
        let uri = Uri::from_str("file:///ws/win.otui").expect("uri");
        let text = "Panel\n\twidth: 10\n"; // tab indentation: a real, expected diagnostic
        let (tx, rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        backend.did_open(did_open_params(&uri, "otui", text));

        let codes = drain_diagnostic_codes(&rx, &uri);
        assert!(
            codes.contains(&"tab-indentation".to_owned()),
            "an .otui document must still be analyzed: {codes:?}"
        );

        let hover = backend.hover(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(0, 2),
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        });
        assert!(
            hover.is_some(),
            "hover must still answer for an .otui document"
        );
    }

    #[test]
    fn an_open_lua_buffer_never_reaches_the_otui_aggregators() {
        // Guarding the *requesting* document is not enough. `collect_references` and
        // `build_rename_edits` fan out over every document in `merged_documents()` and run the OTUI
        // parser on each. And the OTUI grammar does not politely reject Lua: a column-0 line like
        // `x < limit)` — the tail of a wrapped expression — parses as a `style_header` named `x`.
        //
        // So if an open `.lua` buffer reached those aggregators, renaming an OTUI style `x` would
        // emit a `TextEdit` **rewriting bytes inside the user's open Lua file**. This test pins both
        // halves: that the danger is real, and that the seam keeps Lua out.
        let lua_src = "local ok = check(alpha,\nx < limit)\n";

        // (a) the danger is real — the OTUI parser genuinely mistakes this Lua for a style header.
        let svc = OtuiService::new();
        assert!(
            svc.style_defs(lua_src).iter().any(|def| def.name == "x"),
            "precondition failed: if the parser no longer reads this Lua as a style header, \
             rewrite this test around a shape that it does — do not delete it"
        );

        // (b) the seam keeps it out.
        let lua_uri = Uri::from_str("file:///ws/mod.lua").expect("uri");
        let otui_uri = Uri::from_str("file:///ws/mod.otui").expect("uri");
        let mut open = HashMap::new();
        open.insert(
            lua_uri.clone(),
            Document {
                text: lua_src.to_owned(),
                version: 1,
                language: Language::Lua,
            },
        );
        open.insert(
            otui_uri.clone(),
            Document {
                text: "x < UIWidget\n".to_owned(),
                version: 1,
                language: Language::Otui,
            },
        );

        let merged = merge_documents(&open, &HashMap::new());
        assert!(
            !merged.contains_key(&lua_uri),
            "a Lua buffer must never enter the OTUI aggregators — rename would corrupt it"
        );
        assert!(
            merged.contains_key(&otui_uri),
            "the OTUI buffer must still be there"
        );
    }

    #[test]
    fn a_lua_document_never_enters_the_merge_via_the_disk_half_either() {
        // The open half was filtered first, and that looked like enough. It was not: `disk_texts` is
        // seeded by `index_from_disk`, whose "only `.otui` gets here" invariant is enforced by string
        // tests in *other* functions — one of which (the watched-files router) matched `.lua`
        // case-sensitively while `Language::from_uri` did not, so a `Mod.LUA` walked straight in.
        // A seam that trusts an invariant enforced elsewhere is not a seam; this one re-checks.
        let lua_uri = Uri::from_str("file:///ws/Mod.LUA").expect("uri");
        let otui_uri = Uri::from_str("file:///ws/win.otui").expect("uri");
        let mut disk = HashMap::new();
        disk.insert(
            lua_uri.clone(),
            "local ok = check(alpha,\nx < limit)\n".to_owned(),
        );
        disk.insert(otui_uri.clone(), "x < UIWidget\n".to_owned());

        let merged = merge_documents(&HashMap::new(), &disk);
        assert!(
            !merged.contains_key(&lua_uri),
            "a Lua file must not reach the OTUI aggregators through the disk half either"
        );
        assert!(merged.contains_key(&otui_uri));
    }

    #[test]
    fn the_language_classifier_never_fails_open_on_a_lua_uri() {
        // Every miss here fails OPEN: the OTUI analyzer runs over Lua text and `style_defs` feeds the
        // workspace index whatever it hallucinates out of it. `%2E` is the case that bit us — the
        // classifier read the raw string (no literal dot ⇒ "OTUI") while the file reader
        // percent-decoded it and opened the real Lua file.
        for s in [
            "file:///ws/mod.lua",
            "file:///ws/Mod.LUA",
            "file:///ws/mod.Lua",
            "file:///ws/mod%2Elua",
            "file:///ws/mod%2elua",
            "file:///ws/mod.lua#frag",
            "file:///ws/mod.lua?x=1",
        ] {
            let uri = Uri::from_str(s).expect("uri");
            assert_eq!(Language::from_uri(&uri), Language::Lua, "{s} must be Lua");
        }
        // The other direction fails closed (the file silently loses every LSP feature) — milder, but
        // still a bug.
        for s in [
            "file:///ws/win.otui",
            "file:///ws/a.lua/win.otui",
            "file:///ws/WIN.OTUI",
        ] {
            let uri = Uri::from_str(s).expect("uri");
            assert_eq!(Language::from_uri(&uri), Language::Otui, "{s} must be OTUI");
        }
    }

    #[test]
    fn a_watched_lua_file_never_reaches_the_style_index_or_disk_texts() {
        // The end-to-end test that the previous one only pretended to be. That one carried the
        // router's name but asserted on `Language::from_uri` — so reverting the router to its
        // original case-sensitive `ends_with(".lua")` bug left the entire suite green. A test that
        // passes for the wrong reason is worse than no test: it certifies what it never checks.
        //
        // This drives the real handler against real files and asserts on the real state. The Lua text
        // is a wrapped expression whose second line — `x < limit)` — the OTUI grammar reads as a style
        // header named `x`. So if any of it leaks, `style_index.lookup("x")` finds it, and from the
        // index a rename would rewrite bytes inside that Lua file.
        use lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let base = std::env::temp_dir().join(format!("otui-lua-router-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp dir");
        let lua_text = "local ok = check(alpha,\nx < limit)\n";
        for name in ["mod.lua", "Mixed.LUA", "encoded.lua"] {
            std::fs::write(base.join(name), lua_text).expect("write lua");
        }

        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());

        let mut uris: Vec<Uri> = ["mod.lua", "Mixed.LUA"]
            .iter()
            .map(|n| uri_from_file_path(&base.join(n)).expect("uri"))
            .collect();
        // The percent-encoded spelling of the same path: legal per RFC 3986, and the shape that
        // walked past the raw-string classifier straight into the index.
        let encoded = uri_from_file_path(&base.join("encoded.lua")).expect("uri");
        uris.push(
            Uri::from_str(&encoded.as_str().replace("encoded.lua", "encoded%2Elua")).expect("uri"),
        );

        backend.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: uris
                .iter()
                .map(|uri| FileEvent {
                    uri: uri.clone(),
                    typ: FileChangeType::CREATED,
                })
                .collect(),
        });

        let disk = backend.disk_texts.read().expect("disk_texts lock");
        assert!(
            disk.is_empty(),
            "a Lua file must never be cached as OTUI disk text: {:?}",
            disk.keys().collect::<Vec<_>>()
        );
        drop(disk);
        let styles = backend.style_index.read().expect("style_index lock");
        assert!(
            styles.lookup("x").is_empty(),
            "the OTUI parser must never have run over the Lua text"
        );
        drop(styles);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_lua_uri_is_lua_even_when_the_client_labels_it_otherwise() {
        // The guard must not hinge on a `languageId` string chosen in a *different repository*. A
        // client that opens a `.lua` file as "plaintext" (or anything else) must not switch the
        // whole guard off: either signal saying Lua is enough.
        let lua = Uri::from_str("file:///ws/mod.lua").expect("uri");
        let otui = Uri::from_str("file:///ws/mod.otui").expect("uri");
        assert_eq!(Language::classify(&lua, "plaintext"), Language::Lua);
        assert_eq!(Language::classify(&lua, ""), Language::Lua);
        assert_eq!(Language::classify(&lua, "lua"), Language::Lua);
        // And the languageId alone is still enough, whatever the URI looks like.
        assert_eq!(Language::classify(&otui, "lua"), Language::Lua);
        // An OTUI buffer stays OTUI under any of the ids clients actually send.
        assert_eq!(Language::classify(&otui, "otui"), Language::Otui);
        assert_eq!(Language::classify(&otui, "otml"), Language::Otui);
        assert_eq!(Language::classify(&otui, ""), Language::Otui);
        // Case-insensitive on the extension.
        let upper = Uri::from_str("file:///ws/MOD.LUA").expect("uri");
        assert_eq!(Language::classify(&upper, ""), Language::Lua);
    }

    #[test]
    fn did_close_reindexes_from_disk_text_via_the_pure_path() {
        // Simulate the did_close semantics on the pure indexing path: a doc that was open is closed,
        // so it is re-indexed from its *disk* text (fed here directly). The closed file stays in the
        // index and its span still resolves against the cached disk text.
        let uri = Uri::from_str("file:///a.otui").expect("uri");
        let disk_text = "Panel < UIWidget\n"; // what is on disk at close time
        let svc = OtuiService::new();
        let mut index = StyleIndex::new();
        index.set_document(DocId::from(uri.to_string()), svc.style_defs(disk_text));
        let mut disk = HashMap::new();
        disk.insert(uri.clone(), disk_text.to_owned());

        // No open buffers now (the file was closed): the merged view is disk-only.
        let documents = merge_documents(&HashMap::new(), &disk);
        let resp = resolve_base_definition(&index, &documents, "Panel", PositionEncoding::Utf16)
            .expect("closed file still resolves");
        match resp {
            GotoDefinitionResponse::Scalar(loc) => assert_eq!(loc.uri, uri),
            other => panic!("expected a scalar location, got {other:?}"),
        }
    }

    #[test]
    fn indexing_an_unparseable_or_binary_text_adds_no_bogus_entries() {
        // A garbage/binary-looking string must never crash extraction nor land spurious style defs:
        // it simply contributes no `Name < Base` headers.
        let svc = OtuiService::new();
        let mut index = StyleIndex::new();
        let uri = Uri::from_str("file:///junk.otui").expect("uri");
        // Replacement char + NUL + brackets, and crucially no top-level `Name < Base` header.
        let junk = "\u{fffd}\u{0}not-a-style {{{ ][ \n\t\t garbage bytes";
        // Extraction is total: it returns whatever headers it finds (here, none) without panicking.
        let defs = svc.style_defs(junk);
        index.set_document(DocId::from(uri.to_string()), defs);
        // No top-level `Name < Base` header → no entries for this document.
        assert!(
            index
                .document(&DocId::from(uri.to_string()))
                .is_none_or(<[StyleDef]>::is_empty)
        );
        // And a lookup of anything finds nothing from it.
        assert!(index.lookup("garbage").is_empty());
    }

    #[test]
    fn scan_workspace_indexes_otui_and_skips_binary_and_non_otui() {
        // A thin end-to-end check of the disk seam (walk + read + filters) against a real temp tree.
        let base = std::env::temp_dir().join(format!("otui-scan-{}", std::process::id()));
        let nested = base.join("sub");
        std::fs::create_dir_all(&nested).expect("mkdir");
        // A good style file (nested, to exercise recursion).
        std::fs::write(nested.join("good.otui"), "Panel < UIWidget\n").expect("write good");
        // A binary `.otui` (invalid UTF-8) must be skipped, not crash the walk.
        std::fs::write(base.join("binary.otui"), [0xff, 0xfe, 0x00, 0x01]).expect("write binary");
        // A non-`.otui` file is ignored entirely.
        std::fs::write(base.join("notes.txt"), "Ignore < UIWidget\n").expect("write txt");

        let root = uri_from_file_path(&base).expect("root url");
        let mut entries = scan_workspace(&[root]);
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        // Only the good, valid-UTF-8 `.otui` file comes back.
        assert_eq!(
            entries.len(),
            1,
            "only good.otui is indexed, got {entries:?}"
        );
        assert!(entries[0].0.as_str().ends_with("good.otui"));
        assert_eq!(entries[0].1, "Panel < UIWidget\n");

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn scan_workspace_lua_collects_lua_and_scans_widgets() {
        // The `.lua` walk mirrors the `.otui` one but feeds the widget scanner: only `.lua` files
        // are returned, and the scanner extracts the widget's parent + custom props.
        let base = std::env::temp_dir().join(format!("otui-lua-scan-{}", std::process::id()));
        let nested = base.join("ui");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(
            nested.join("uitable.lua"),
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
        // An `.otui` sibling must not be collected by the lua walk.
        std::fs::write(base.join("styles.otui"), "Panel < UIWidget\n").expect("write otui");

        let root = uri_from_file_path(&base).expect("root url");
        let entries = scan_workspace_lua(&[root]);
        assert_eq!(
            entries.len(),
            1,
            "only the .lua file is collected: {entries:?}"
        );
        assert!(entries[0].0.as_str().ends_with("uitable.lua"));

        let defs = OtuiService::new().lua_widgets(&entries[0].1);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "UITable");
        assert_eq!(defs[0].lua_parent.as_deref(), Some("UIWidget"));
        assert!(defs[0].custom_props.contains("column-style"));

        std::fs::remove_dir_all(&base).ok();
    }

    /// Drain every pending `publishDiagnostics` notification and return the diagnostic codes of the
    /// last one addressed to `uri` (empty if none was sent).
    fn drain_diagnostic_codes(rx: &crossbeam_channel::Receiver<Message>, uri: &Uri) -> Vec<String> {
        let mut codes = None;
        while let Ok(msg) = rx.try_recv() {
            if let Message::Notification(note) = msg
                && note.method == "textDocument/publishDiagnostics"
            {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(note.params).expect("diagnostics params");
                if &params.uri == uri {
                    codes = Some(
                        params
                            .diagnostics
                            .iter()
                            .filter_map(|d| match &d.code {
                                Some(lsp_types::NumberOrString::String(s)) => Some(s.clone()),
                                _ => None,
                            })
                            .collect(),
                    );
                }
            }
        }
        codes.unwrap_or_default()
    }

    #[test]
    fn watched_lua_change_republishes_open_documents() {
        use lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let base = std::env::temp_dir().join(format!("otui-lua-republish-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("mkdir");
        let lua_path = base.join("uitable.lua");
        let lua_uri = uri_from_file_path(&lua_path).expect("lua uri");

        let (tx, rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());

        // An open `.otui` that puts a Lua-added property (`column-style`) on a `UITable`.
        let doc_uri = Uri::from_str("file:///ws/win.otui").expect("doc uri");
        let text = "Table < UITable\n  column-style: SomeColumn\n";
        backend.documents.write().expect("documents").insert(
            doc_uri.clone(),
            Document {
                text: text.to_owned(),
                version: 1,
                language: Language::Otui,
            },
        );

        // Before the Lua file exists/indexed: the property is unknown → hint.
        backend.publish(doc_uri.clone(), text, 1);
        assert!(
            drain_diagnostic_codes(&rx, &doc_uri)
                .iter()
                .any(|c| c == "unknown-property"),
            "column-style should hint before UITable's lua is indexed"
        );

        // Now the Lua module appears on disk and a watched-file event fires.
        std::fs::write(
            &lua_path,
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
        backend.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: lua_uri,
                typ: FileChangeType::CREATED,
            }],
        });

        // The open document was republished and the hint is gone.
        assert!(
            !drain_diagnostic_codes(&rx, &doc_uri)
                .iter()
                .any(|c| c == "unknown-property"),
            "column-style must be accepted after UITable's lua is indexed"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn initial_scan_refreshes_open_documents_on_completion() {
        use std::time::Duration;

        // A workspace whose Lua declares UITable's column-style.
        let base = std::env::temp_dir().join(format!("otui-scan-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(
            base.join("uitable.lua"),
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

        let (tx, rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        *backend.roots.lock().expect("roots") = vec![uri_from_file_path(&base).expect("root")];

        // A document open *before* the scan runs, using the Lua-added property.
        let doc_uri = Uri::from_str("file:///ws/win.otui").expect("doc uri");
        backend.documents.write().expect("documents").insert(
            doc_uri.clone(),
            Document {
                text: "Table < UITable\n  column-style: SomeColumn\n".to_owned(),
                version: 1,
                language: Language::Otui,
            },
        );

        // Spawn the background scan; its completion refresh should re-diagnose the open document.
        backend.run_initialized();

        // Wait (bounded) for a publishDiagnostics addressed to the open document.
        let mut refreshed = false;
        while let Ok(msg) = rx.recv_timeout(Duration::from_secs(5)) {
            if let Message::Notification(note) = msg
                && note.method == "textDocument/publishDiagnostics"
            {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(note.params).expect("diagnostics params");
                if params.uri == doc_uri {
                    refreshed = !params.diagnostics.iter().any(|d| {
                            matches!(&d.code, Some(lsp_types::NumberOrString::String(s)) if s == "unknown-property")
                        });
                    break;
                }
            }
        }
        backend.signal_shutdown();
        assert!(
            refreshed,
            "the initial scan's completion refresh should clear the stale column-style hint"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    #[allow(deprecated)] // exercises the mandatory-but-deprecated `root_uri` fallback path.
    fn workspace_roots_prefers_folders_then_falls_back_to_root_uri() {
        use lsp_types::WorkspaceFolder;
        // workspace_folders present → its URIs win over root_uri.
        let params = InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: Uri::from_str("file:///ws/").expect("uri"),
                name: "ws".to_owned(),
            }]),
            root_uri: Some(Uri::from_str("file:///legacy/").expect("uri")),
            ..InitializeParams::default()
        };
        assert_eq!(
            workspace_roots(&params),
            vec![Uri::from_str("file:///ws/").expect("uri")]
        );

        // No folders → the legacy root_uri is used.
        let params = InitializeParams {
            workspace_folders: None,
            root_uri: Some(Uri::from_str("file:///legacy/").expect("uri")),
            ..InitializeParams::default()
        };
        assert_eq!(
            workspace_roots(&params),
            vec![Uri::from_str("file:///legacy/").expect("uri")]
        );

        // Neither → empty (fall back to open-docs-only indexing).
        assert!(workspace_roots(&InitializeParams::default()).is_empty());
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
        // from the instance lands on the declaration's name span (resolved from the cached index).
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "Panel < UIWidget\n"),
            (
                "file:///use.otui",
                "MainWindow < UIWindow\n  Panel\n    id: p\n",
            ),
        ]);
        let resp = resolve_type_definition(&index, &docs, "Panel", PositionEncoding::Utf16)
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
        // `UIWidget` is native: no user declaration in the index → `None`.
        let (index, docs) = workspace(&[("file:///a.otui", "Panel < UIWidget\n")]);
        assert!(
            resolve_type_definition(&index, &docs, "UIWidget", PositionEncoding::Utf16).is_none()
        );
    }

    #[test]
    fn type_definition_with_duplicate_decls_is_an_array() {
        // The same style declared in two files: typeDefinition surfaces both declaration sites.
        let (index, docs) = workspace(&[
            ("file:///a.otui", "Dup < UIWidget\n"),
            ("file:///b.otui", "Dup < UIWindow\n"),
        ]);
        let resp = resolve_type_definition(&index, &docs, "Dup", PositionEncoding::Utf16)
            .expect("resolves");
        match resp {
            GotoDefinitionResponse::Array(locs) => assert_eq!(locs.len(), 2),
            other => panic!("expected an array of locations, got {other:?}"),
        }
    }

    #[test]
    fn implementation_lists_derivations_across_two_docs() {
        // `Base` is derived from in two separate files; implementation aggregates both from the index.
        let (index, docs) = workspace(&[
            ("file:///base.otui", "Base < UIWidget\n"),
            ("file:///a.otui", "ChildA < Base\n"),
            ("file:///b.otui", "ChildB < Base\n"),
        ]);
        let locs = collect_implementations(&index, &docs, "Base", PositionEncoding::Utf16);
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
        let (index, docs) = workspace(&[("file:///a.otui", "Leaf < UIWidget\n")]);
        assert!(collect_implementations(&index, &docs, "Leaf", PositionEncoding::Utf16).is_empty());
    }

    // --- Type hierarchy (prepareTypeHierarchy / supertypes / subtypes) ---

    #[test]
    fn prepare_roots_the_hierarchy_on_the_style_under_the_cursor() {
        // `Panel` is declared in one file and used as an instance in another; prepare roots on the
        // declaration, carrying its name, uri, header/name ranges, base detail, and name data.
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "Panel < UIWidget\n  id: p\n"),
            (
                "file:///use.otui",
                "MainWindow < UIWindow\n  Panel\n    id: p\n",
            ),
        ]);
        let item = prepare_type_hierarchy_item(&index, &docs, "Panel", PositionEncoding::Utf16)
            .expect("roots on the declaration");
        assert_eq!(item.name, "Panel");
        assert_eq!(item.kind, SymbolKind::CLASS);
        assert_eq!(item.uri.as_str(), "file:///defs.otui");
        // detail carries the base, like the hover's "inherits from".
        assert_eq!(item.detail.as_deref(), Some("UIWidget"));
        // selection_range is the name token; range covers the whole header (declaration + body).
        assert_eq!(item.selection_range.start, Position::new(0, 0));
        assert_eq!(item.selection_range.end, Position::new(0, 5));
        assert_eq!(item.range.start, Position::new(0, 0));
        // The header range extends over the indented body, past the declaration line.
        assert!(item.range.end.line >= 1);
        // data round-trips the style name.
        assert_eq!(item_style_name(&item), "Panel");
    }

    #[test]
    fn prepare_is_none_for_a_native_or_unknown_name() {
        let (index, docs) = workspace(&[("file:///a.otui", "Panel < UIWidget\n")]);
        // A native `UI*` name has no user declaration to root on.
        assert!(
            prepare_type_hierarchy_item(&index, &docs, "UIWidget", PositionEncoding::Utf16)
                .is_none()
        );
        // A name declared nowhere in the workspace.
        assert!(
            prepare_type_hierarchy_item(&index, &docs, "Missing", PositionEncoding::Utf16)
                .is_none()
        );
    }

    #[test]
    fn prepare_roots_on_the_first_declaration_when_duplicated() {
        // The same style declared in two files is legal; prepare roots on a single (the first)
        // declaration rather than returning several roots.
        let (index, docs) = workspace(&[
            ("file:///a.otui", "Dup < UIWidget\n"),
            ("file:///b.otui", "Dup < UIWindow\n"),
        ]);
        let item = prepare_type_hierarchy_item(&index, &docs, "Dup", PositionEncoding::Utf16)
            .expect("roots on one declaration");
        assert_eq!(item.name, "Dup");
        // It is one of the two declarations (order across docs is unspecified).
        assert!(["file:///a.otui", "file:///b.otui"].contains(&item.uri.as_str()));
    }

    #[test]
    fn supertypes_returns_the_user_base_item() {
        // `Child < Base`, `Base < UIWidget`: the direct supertype of `Child` is the user style `Base`.
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "Base < UIWidget\n"),
            ("file:///use.otui", "Child < Base\n"),
        ]);
        let supers = resolve_supertypes(&index, &docs, "Child", PositionEncoding::Utf16);
        assert_eq!(supers.len(), 1);
        assert_eq!(supers[0].name, "Base");
        assert_eq!(supers[0].uri.as_str(), "file:///defs.otui");
        assert_eq!(supers[0].detail.as_deref(), Some("UIWidget"));
        assert_eq!(item_style_name(&supers[0]), "Base");
    }

    #[test]
    fn supertypes_of_a_native_base_is_empty_chain_end() {
        // `Panel < UIWidget`: its base is native `UIWidget`, a built-in leaf — the chain ends here.
        let (index, docs) = workspace(&[("file:///a.otui", "Panel < UIWidget\n")]);
        assert!(resolve_supertypes(&index, &docs, "Panel", PositionEncoding::Utf16).is_empty());
    }

    #[test]
    fn supertypes_of_a_dangling_base_is_empty() {
        // `Child < Missing` where `Missing` is declared nowhere: no navigable supertype.
        let (index, docs) = workspace(&[("file:///a.otui", "Child < Missing\n")]);
        assert!(resolve_supertypes(&index, &docs, "Child", PositionEncoding::Utf16).is_empty());
    }

    #[test]
    fn subtypes_returns_an_item_per_deriving_style_across_docs() {
        // `Base` is derived from in two separate files; subtypes lists both.
        let (index, docs) = workspace(&[
            ("file:///base.otui", "Base < UIWidget\n"),
            ("file:///a.otui", "ChildA < Base\n"),
            ("file:///b.otui", "ChildB < Base\n"),
        ]);
        let mut subs = resolve_subtypes(&index, &docs, "Base", PositionEncoding::Utf16);
        subs.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].name, "ChildA");
        assert_eq!(subs[0].detail.as_deref(), Some("Base"));
        assert_eq!(subs[1].name, "ChildB");
        assert_eq!(item_style_name(&subs[0]), "ChildA");
    }

    #[test]
    fn subtypes_is_empty_when_nothing_derives() {
        let (index, docs) = workspace(&[("file:///a.otui", "Leaf < UIWidget\n")]);
        assert!(resolve_subtypes(&index, &docs, "Leaf", PositionEncoding::Utf16).is_empty());
    }

    #[test]
    fn item_data_round_trips_the_name_through_supertypes_and_subtypes() {
        // Build the root item exactly as prepare does, then drive supertypes/subtypes off *that*
        // item's carried name — the client always passes the item back, never a bare name.
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "Base < UIWidget\n"),
            ("file:///mid.otui", "Mid < Base\n"),
            ("file:///leaf.otui", "Leaf < Mid\n"),
        ]);
        let mid = prepare_type_hierarchy_item(&index, &docs, "Mid", PositionEncoding::Utf16)
            .expect("Mid is declared");
        // supertypes(Mid) via the item → Base.
        let supers = resolve_supertypes(
            &index,
            &docs,
            &item_style_name(&mid),
            PositionEncoding::Utf16,
        );
        assert_eq!(supers.len(), 1);
        assert_eq!(supers[0].name, "Base");
        // subtypes(Mid) via the item → Leaf.
        let subs = resolve_subtypes(
            &index,
            &docs,
            &item_style_name(&mid),
            PositionEncoding::Utf16,
        );
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].name, "Leaf");
    }

    #[test]
    fn item_style_name_falls_back_to_name_when_data_is_absent() {
        // A client that echoes an item without `data` still resolves via the item's `name`.
        let item = TypeHierarchyItem {
            name: "Fallback".to_owned(),
            kind: SymbolKind::CLASS,
            tags: None,
            detail: None,
            uri: Uri::from_str("file:///a.otui").expect("uri"),
            range: Range::default(),
            selection_range: Range::default(),
            data: None,
        };
        assert_eq!(item_style_name(&item), "Fallback");
    }

    #[test]
    fn full_flow_from_cursor_to_type_hierarchy_root() {
        // Position → offset → style_type_at → prepare, the path the handler drives.
        let (index, docs) = workspace(&[
            ("file:///defs.otui", "Panel < UIWidget\n"),
            (
                "file:///use.otui",
                "MainWindow < UIWindow\n  Panel\n    id: p\n",
            ),
        ]);
        let request_text = "MainWindow < UIWindow\n  Panel\n    id: p\n";
        // Cursor on the `Panel` widget instance (line 1, column 2).
        let position = Position::new(1, 2);
        let offset = LineIndex::new(request_text).offset_at(position, PositionEncoding::Utf16);
        let type_ref = OtuiService::new()
            .style_type_at(request_text, offset)
            .expect("cursor is on the instance tag");
        assert_eq!(type_ref.name, "Panel");
        let item =
            prepare_type_hierarchy_item(&index, &docs, &type_ref.name, PositionEncoding::Utf16)
                .expect("roots on Panel's declaration");
        assert_eq!(item.uri.as_str(), "file:///defs.otui");
    }

    /// The `(uri, range)` of each location, sorted, for order-independent assertions (the document
    /// store iterates an unordered map).
    fn sorted_locs(locs: &[Location]) -> Vec<(String, Position, Position)> {
        let mut out: Vec<(String, Position, Position)> = locs
            .iter()
            .map(|l| (l.uri.as_str().to_string(), l.range.start, l.range.end))
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
    fn paired_uri_swaps_the_extension_in_place() {
        let otui = Uri::from_str("file:///ws/modules/login/login.otui").expect("uri");
        let lua = paired_uri(&otui, "lua").expect("paired");
        assert_eq!(lua.as_str(), "file:///ws/modules/login/login.lua");

        // And the reverse direction.
        let back = paired_uri(&lua, "otui").expect("paired back");
        assert_eq!(back.as_str(), otui.as_str());
    }

    #[test]
    fn paired_uri_preserves_a_stem_that_itself_contains_dots() {
        // `with_extension` must only ever touch the LAST extension — a real corpus filename
        // (`30-miniwindow.otui`) has a dot in its stem, and swapping the whole thing after the
        // first dot would pair it with the wrong (nonexistent) file.
        let otui = Uri::from_str("file:///ws/styles/30-miniwindow.otui").expect("uri");
        let lua = paired_uri(&otui, "lua").expect("paired");
        assert_eq!(lua.as_str(), "file:///ws/styles/30-miniwindow.lua");
    }

    #[test]
    fn paired_uri_round_trips_a_percent_encoded_space_in_the_directory() {
        // A workspace path containing a space (`%20`) is ordinary and legal; pairing must decode it
        // the same way the reader that indexes the file does (`uri_to_file_path`), not disagree with
        // it — see `paired_uri`'s doc comment.
        let otui =
            Uri::from_str("file:///Users/dev/My%20Client/modules/login/login.otui").expect("uri");
        let lua = paired_uri(&otui, "lua").expect("paired");
        // Re-encoded back through the `url` crate, so compare the decoded path, not the raw string
        // (which may legally re-encode the space differently, e.g. as `%20` either way).
        let path = uri_to_file_path(&lua).expect("decodes");
        assert_eq!(
            path,
            PathBuf::from("/Users/dev/My Client/modules/login/login.lua")
        );
    }

    #[test]
    fn paired_uri_handles_a_percent_encoded_dot_in_the_original_name() {
        // `%2E` is a legal percent-encoding of a literal `.`; the pairing must decode it before
        // splitting on the extension, exactly like `Language::from_uri` must (see its doc comment
        // for the bug class this guards against: a raw-string test disagreeing with the decoding
        // reader).
        let otui = Uri::from_str("file:///ws/login%2Eotui").expect("uri");
        let lua = paired_uri(&otui, "lua").expect("paired");
        assert_eq!(
            uri_to_file_path(&lua).expect("decodes"),
            PathBuf::from("/ws/login.lua")
        );
    }

    #[test]
    fn paired_uri_is_none_for_a_non_file_scheme() {
        let untitled = Uri::from_str("untitled:Untitled-1").expect("uri");
        assert!(paired_uri(&untitled, "lua").is_none());
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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

    /// `(start, end, kind)` of each highlight, sorted by position, for order-independent asserts
    /// (the finders return declarations before refs, but ids/anchors nest at any depth).
    fn sorted_highlights(
        hls: &[DocumentHighlight],
    ) -> Vec<(Position, Position, DocumentHighlightKind)> {
        let mut out: Vec<(Position, Position, DocumentHighlightKind)> = hls
            .iter()
            .map(|h| (h.range.start, h.range.end, h.kind.expect("kind set")))
            .collect();
        out.sort_by_key(|a| (a.0.line, a.0.character));
        out
    }

    /// Classify the cursor at the first occurrence of `needle` and collect its document-local
    /// highlights — the exact path the `document_highlight` handler drives, minus the doc store.
    fn highlights_at(src: &str, needle: &str) -> Vec<DocumentHighlight> {
        let svc = OtuiService::new();
        let off = src.find(needle).expect("needle present");
        let target = classify_reference_target(&svc, src, off).expect("on a symbol");
        collect_document_highlights(&target, src, &svc, PositionEncoding::Utf16)
    }

    #[test]
    fn document_highlight_on_a_style_name_marks_declaration_write_and_base_refs_read() {
        // `Base` is declared once (WRITE) and used as a base twice (READ), all in one document.
        // The unrelated `Other` declaration must not be highlighted.
        let src = "Base < UIWidget\nChildA < Base\nChildB < Base\nOther < UIWidget\n";
        let hls = highlights_at(src, "Base");
        assert_eq!(
            sorted_highlights(&hls),
            vec![
                // Declaration: `Base` on line 0 → WRITE.
                (
                    Position::new(0, 0),
                    Position::new(0, 4),
                    DocumentHighlightKind::WRITE
                ),
                // Base refs: `Base` on lines 1 and 2 → READ.
                (
                    Position::new(1, 9),
                    Position::new(1, 13),
                    DocumentHighlightKind::READ
                ),
                (
                    Position::new(2, 9),
                    Position::new(2, 13),
                    DocumentHighlightKind::READ
                ),
            ]
        );
    }

    #[test]
    fn document_highlight_on_an_id_marks_declaration_write_and_anchor_refs_read() {
        let src = "Panel\n  id: header\nOther\n  anchors.top: header.bottom\n  anchors.left: header.left\n";
        // Cursor on the `id:` value declaration.
        let hls = highlights_at(src, "header");
        assert_eq!(
            sorted_highlights(&hls),
            vec![
                // The `id: header` declaration → WRITE.
                (
                    Position::new(1, 6),
                    Position::new(1, 12),
                    DocumentHighlightKind::WRITE
                ),
                // Each `<id>.edge` anchor prefix → READ (span covers just `header`, not `.edge`).
                (
                    Position::new(3, 15),
                    Position::new(3, 21),
                    DocumentHighlightKind::READ
                ),
                (
                    Position::new(4, 16),
                    Position::new(4, 22),
                    DocumentHighlightKind::READ
                ),
            ]
        );
    }

    #[test]
    fn document_highlight_ignores_a_dotted_magic_anchor_target_prefix() {
        // `parent.bottom` references the magic parent widget, not the `id: parent`; only the real
        // declaration is highlighted (reusing the finders' existing magic-target exclusion).
        let src = "Panel\n  id: parent\n  anchors.top: parent.bottom\n";
        // First `parent` occurrence is the `id:` value token (cursor on the declaration).
        let hls = highlights_at(src, "parent");
        assert_eq!(
            sorted_highlights(&hls),
            vec![(
                Position::new(1, 6),
                Position::new(1, 12),
                DocumentHighlightKind::WRITE
            )],
            "the dotted magic target's `parent` prefix is not an id reference"
        );
    }

    #[test]
    fn classify_reference_target_is_none_off_a_symbol() {
        // A property value is neither a style name nor an id → the shared classifier answers `None`,
        // so the reference/highlight handlers that build on it have nothing to resolve.
        let svc = OtuiService::new();
        let src = "Panel\n  width: 10\n";
        let off = src.find("10").expect("present");
        assert!(classify_reference_target(&svc, src, off).is_none());
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
        let defs = &changes[&Uri::from_str("file:///defs.otui").expect("uri")];
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].new_text, "Renamed");
        assert_eq!(defs[0].range.start, Position::new(0, 0));
        assert_eq!(defs[0].range.end, Position::new(0, 7));
        // Each base reference (`ChildX < MyPanel`) is rewritten at columns 9..16.
        for name in ["file:///a.otui", "file:///b.otui"] {
            let e = &changes[&Uri::from_str(name).expect("uri")];
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
        let target = ReferenceTarget::StyleName("MyPanel".to_owned());
        // A name containing a space is not a valid identifier → an `Err(message)` (which the
        // dispatch arm turns into a JSON-RPC `InvalidParams` error), never an edit.
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
        assert!(
            err.contains("not a valid OTML name"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn rename_of_a_native_base_is_refused() {
        // `UIWidget` is a native built-in with no user definition → no declaration to rename.
        let (index, docs) = workspace(&[("file:///a.otui", "MyPanel < UIWidget\n")]);
        let svc = OtuiService::new();
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
        let uri = Uri::from_str("file:///use.otui").expect("uri");
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
        assert!(
            OtuiService::new()
                .style_hover_at(src, offset, &index)
                .is_none()
        );
    }

    /// Render the property-key hover at `needle` in `text` (the fallback path of the hover handler).
    fn property_hover_text(text: &str, needle: &str) -> String {
        let offset = text.find(needle).expect("needle present") + 1;
        let desc = OtuiService::new()
            .property_hover_at(text, offset)
            .expect("cursor is on a known property key");
        let line_index = LineIndex::new(text);
        let h = render_property_hover(&desc, &line_index, PositionEncoding::Utf16);
        match h.contents {
            HoverContents::Markup(m) => m.value,
            other => panic!("expected markup, got {other:?}"),
        }
    }

    #[test]
    fn hover_on_an_asset_path_property_describes_it() {
        let t = property_hover_text("Panel\n  image-source: /images/ui/x\n", "image-source");
        assert!(t.contains("**`image-source`**"), "{t}");
        // Curated behavior for image-source mentions the texture path.
        assert!(t.contains("texture path"), "{t}");
    }

    #[test]
    fn hover_on_an_enum_property_lists_its_values() {
        let t = property_hover_text("Panel\n  display: flex\n", "display");
        assert!(t.contains("**`display`**"), "{t}");
        // Enum properties always append the full accepted value list.
        assert!(t.contains("One of:"), "{t}");
        assert!(t.contains("`flex`"), "{t}");
    }

    #[test]
    fn hover_on_a_color_property_describes_it() {
        let t = property_hover_text("Panel\n  color: red\n", "color");
        // Curated behavior for color describes the draw color.
        assert!(t.contains("**`color`**") && t.contains("draw color"), "{t}");
    }

    #[test]
    fn hover_on_a_known_uncurated_property_uses_the_value_kind_fallback() {
        // `min-width` is a real catalog property with no curated doc → the plain value-kind fallback.
        let t = property_hover_text("Panel\n  min-width: 10\n", "min-width");
        assert!(
            t.contains("**`min-width`**") && t.contains("OTUI style property"),
            "{t}"
        );
        // `border-color-bottom` is a color property with no curated doc → the color-value fallback.
        let t2 = property_hover_text("Panel\n  border-color-bottom: red\n", "border-color-bottom");
        assert!(t2.contains("a color value"), "{t2}");
    }

    /// The [`CodeAction`] inside a [`CodeActionOrCommand`] (panics if it is a bare command).
    fn as_action(item: &CodeActionOrCommand) -> &CodeAction {
        match item {
            CodeActionOrCommand::CodeAction(a) => a,
            other => panic!("expected a CodeAction, got {other:?}"),
        }
    }

    /// The single `(Uri, Vec<TextEdit>)` change set of an action's workspace edit.
    fn only_change(action: &CodeAction) -> (&Uri, &Vec<TextEdit>) {
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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
        let uri = Uri::from_str("file:///a.otui").expect("uri");
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

    // --- document links -----------------------------------------------------
    //
    // `resolve_asset_candidates`, `is_asset_sentinel_value` and `is_runtime_variable_path` moved to
    // `otui_core::links` (CodeRabbit Finding 4 on PR #51 — pure OTML/OTClient path semantics with no
    // I/O belong in the engine crate, not the transport crate). Their unit tests moved with them;
    // see `otui_core::links::tests`.

    #[test]
    fn detect_client_roots_uses_only_the_docs_own_client_root_not_unrelated_workspace_roots() {
        // Finding 1: a workspace can legitimately contain more than one OTClient install (e.g. two
        // client checkouts side by side as separate workspace folders). A document inside root A must
        // resolve against root A alone — an asset that happens to exist under an unrelated root B
        // must never be folded in, or it could suppress a legitimate `missing-asset` finding for A,
        // or redirect a link/hover to the wrong tree entirely.
        let base = std::env::temp_dir().join(format!(
            "otui-two-client-roots-{}-{}",
            std::process::id(),
            line!()
        ));
        let root_a = base.join("client-a");
        let root_b = base.join("client-b");
        for root in [&root_a, &root_b] {
            std::fs::create_dir_all(root.join("data")).expect("mkdir data");
            std::fs::create_dir_all(root.join("modules")).expect("mkdir modules");
            std::fs::write(root.join("init.lua"), b"-- stand-in\n").expect("init.lua");
        }
        let doc_dir = root_a.join("modules").join("game_x");
        std::fs::create_dir_all(&doc_dir).expect("mkdir doc dir");

        let workspace_roots = vec![root_a.clone(), root_b.clone()];
        let detected = detect_client_roots(Some(&doc_dir), &workspace_roots);
        assert_eq!(
            detected,
            vec![root_a.clone()],
            "a document under root A must resolve against root A alone, not both: {detected:?}"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn detect_client_roots_falls_back_to_scanning_workspace_roots_with_no_doc_client_root() {
        // The preserved fallback: when the document itself has no client root above it (e.g. a
        // request with no real document context), every workspace root is still scanned and
        // deduplicated, exactly as before Finding 1.
        let base = std::env::temp_dir().join(format!(
            "otui-client-root-fallback-{}-{}",
            std::process::id(),
            line!()
        ));
        let root = base.join("client");
        std::fs::create_dir_all(root.join("data")).expect("mkdir data");
        std::fs::create_dir_all(root.join("modules")).expect("mkdir modules");
        std::fs::write(root.join("init.lua"), b"-- stand-in\n").expect("init.lua");
        let unrelated_doc_dir = base.join("nowhere");
        std::fs::create_dir_all(&unrelated_doc_dir).expect("mkdir unrelated");

        let workspace_roots = vec![root.clone()];
        let detected = detect_client_roots(Some(&unrelated_doc_dir), &workspace_roots);
        assert_eq!(detected, vec![root]);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn otpkg_present_under_cached_memoizes_the_walk_per_root() {
        // Finding 3: the same root queried twice must not re-walk the filesystem — proven here by
        // deleting the archive between calls; a cached `true` must survive the deletion (the whole
        // point of memoizing), while a fresh `otpkg_present_under` call would see `false`.
        let base = std::env::temp_dir().join(format!(
            "otui-otpkg-cache-{}-{}",
            std::process::id(),
            line!()
        ));
        let mods = base.join("mods");
        std::fs::create_dir_all(&mods).expect("mkdir");
        let archive = mods.join("bundle.otpkg");
        std::fs::write(&archive, b"not a real zip").expect("write");

        let cache: RwLock<HashMap<PathBuf, bool>> = RwLock::new(HashMap::new());
        assert!(otpkg_present_under_cached(&cache, &base));

        std::fs::remove_file(&archive).expect("remove archive");
        assert!(
            otpkg_present_under_cached(&cache, &base),
            "a cached hit must not be invalidated by a later on-disk change"
        );
        // The uncached function, called fresh, now correctly reports no archive — proving the cache
        // (not some other effect) is what kept the first assertion `true`.
        assert!(!otpkg_present_under(&base));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn find_client_root_walks_up_from_a_nested_directory_to_the_install_root() {
        // Finding 2: the install root is `init.lua` + `data/` + `modules/` together — verified
        // against the real `init.lua` (`g_resources.addSearchPath(getWorkDir() .. 'data' |
        // 'modules', true)`, both unconditional and fatal if missing).
        let base = std::env::temp_dir().join(format!(
            "otui-client-root-{}-{}",
            std::process::id(),
            line!()
        ));
        let nested = base.join("modules").join("game_x");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(base.join("init.lua"), b"-- stand-in\n").expect("init.lua");
        std::fs::create_dir_all(base.join("data")).expect("mkdir data");
        std::fs::create_dir_all(base.join("modules")).expect("mkdir modules");

        assert_eq!(find_client_root(&nested), Some(base.clone()));
        assert_eq!(find_client_root(&base), Some(base.clone()));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn find_client_root_is_none_for_a_standalone_module_with_no_markers_above_it() {
        // The scenario Finding 2 exists for: a module or mod repository opened on its own has no
        // `init.lua`/`data`/`modules` anywhere above it.
        let base = std::env::temp_dir().join(format!(
            "otui-no-client-root-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&base).expect("mkdir");
        assert_eq!(find_client_root(&base), None);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn find_client_root_requires_init_lua_not_just_the_two_directories() {
        // `data/` + `modules/` without `init.lua` is not enough — a coincidental pair of
        // similarly-named directories must not be mistaken for the install root.
        let base = std::env::temp_dir().join(format!(
            "otui-client-root-no-init-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(base.join("data")).expect("mkdir data");
        std::fs::create_dir_all(base.join("modules")).expect("mkdir modules");
        assert_eq!(find_client_root(&base), None);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn otpkg_present_under_finds_an_archive_nested_under_a_subdirectory() {
        let base = std::env::temp_dir().join(format!(
            "otui-otpkg-present-{}-{}",
            std::process::id(),
            line!()
        ));
        let mods = base.join("mods");
        std::fs::create_dir_all(&mods).expect("mkdir");
        std::fs::write(mods.join("bundle.otpkg"), b"not a real zip").expect("write");

        assert!(otpkg_present_under(&base));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn otpkg_present_under_is_false_with_no_archive_anywhere() {
        let base = std::env::temp_dir().join(format!(
            "otui-otpkg-absent-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(base.join("images")).expect("mkdir");
        std::fs::write(base.join("images").join("a.png"), b"png").expect("write");

        assert!(!otpkg_present_under(&base));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn document_link_resolves_an_extensionless_image_source() {
        // The real-world shape: `image-source` written without `.png`, resolving to `<name>.png`.
        let base = std::env::temp_dir().join(format!("otui-links-noext-{}", std::process::id()));
        let assets = base.join("images").join("ui");
        std::fs::create_dir_all(&assets).expect("mkdir");
        std::fs::write(assets.join("button.png"), b"png").expect("write asset");

        let doc_path = base.join("window.otui");
        let doc_uri = uri_from_file_path(&doc_path).expect("doc uri");
        let text = "Panel\n  image-source: images/ui/button\n";
        let backend = backend_with_doc(&doc_uri, text, Vec::new());

        let links = backend
            .document_link(link_params(&doc_uri))
            .expect("known document");
        assert_eq!(
            links.len(),
            1,
            "extensionless source should link, got {links:?}"
        );
        let target = uri_from_file_path(&assets.join("button.png")).expect("target uri");
        assert_eq!(links[0].target.as_ref(), Some(&target));

        std::fs::remove_dir_all(&base).ok();
    }

    /// Build a `Backend` with an open `file://` document and the given workspace roots, for driving
    /// the `document_link` handler directly.
    fn backend_with_doc(uri: &Uri, text: &str, roots: Vec<Uri>) -> Backend {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let params = InitializeParams::default();
        let backend = Backend::new(tx, &params);
        *backend.roots.lock().expect("roots") = roots;
        backend.documents.write().expect("documents").insert(
            uri.clone(),
            Document {
                text: text.to_owned(),
                version: 0,
                language: Language::Otui,
            },
        );
        backend
    }

    fn link_params(uri: &Uri) -> DocumentLinkParams {
        use lsp_types::{PartialResultParams, TextDocumentIdentifier, WorkDoneProgressParams};
        DocumentLinkParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }

    #[test]
    fn document_link_emits_for_existing_target_and_skips_missing() {
        // A real temp tree: one asset that exists (relative to the doc dir) and one that does not.
        let base = std::env::temp_dir().join(format!("otui-links-{}", std::process::id()));
        let assets = base.join("images");
        std::fs::create_dir_all(&assets).expect("mkdir");
        std::fs::write(assets.join("present.png"), b"png").expect("write asset");

        let doc_path = base.join("window.otui");
        let doc_uri = uri_from_file_path(&doc_path).expect("doc uri");
        // Two relative image-source paths: one existing, one missing.
        let text = "Panel\n  image-source: images/present.png\nOther\n  image-source: images/missing.png\n";
        let backend = backend_with_doc(&doc_uri, text, Vec::new());

        let links = backend
            .document_link(link_params(&doc_uri))
            .expect("known document");
        // Only the existing target produces a link (no dead links).
        assert_eq!(links.len(), 1, "got {links:?}");
        let link = &links[0];
        assert_eq!(link.tooltip.as_deref(), Some("Open images/present.png"));
        let target = uri_from_file_path(&assets.join("present.png")).expect("target uri");
        assert_eq!(link.target.as_ref(), Some(&target));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn document_link_resolves_rooted_path_against_workspace_root() {
        // A `/`-rooted path resolves against the workspace root (the heuristic data root).
        let base = std::env::temp_dir().join(format!("otui-links-root-{}", std::process::id()));
        let assets = base.join("data").join("images");
        std::fs::create_dir_all(&assets).expect("mkdir");
        std::fs::write(assets.join("bg.png"), b"png").expect("write asset");

        let root = base.join("data");
        let root_uri = uri_from_file_path(&root).expect("root uri");
        // Document sits somewhere under the project; the `/`-rooted path is data-root relative.
        let doc_path = base.join("modules").join("ui.otui");
        std::fs::create_dir_all(doc_path.parent().unwrap()).expect("mkdir doc");
        let doc_uri = uri_from_file_path(&doc_path).expect("doc uri");
        let text = "Panel\n  image-source: /images/bg.png\n";
        let backend = backend_with_doc(&doc_uri, text, vec![root_uri]);

        let links = backend
            .document_link(link_params(&doc_uri))
            .expect("known document");
        assert_eq!(links.len(), 1, "got {links:?}");
        let target = uri_from_file_path(&assets.join("bg.png")).expect("target uri");
        assert_eq!(links[0].target.as_ref(), Some(&target));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn document_link_on_unknown_document_is_none() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        let uri = Uri::from_str("file:///nope.otui").expect("uri");
        assert!(backend.document_link(link_params(&uri)).is_none());
    }

    #[test]
    fn document_link_on_a_non_file_uri_is_none() {
        // A document open under a non-`file://` scheme has no filesystem path, so links can't be
        // resolved: `uri_to_file_path` returns None early and no links are produced.
        let uri = Uri::from_str("untitled:Untitled-1").expect("uri");
        let backend = backend_with_doc(&uri, "Panel\n  image-source: images/x.png\n", Vec::new());
        assert!(backend.document_link(link_params(&uri)).is_none());
    }

    fn range_formatting_params(
        uri: &Uri,
        range: lsp_types::Range,
    ) -> DocumentRangeFormattingParams {
        use lsp_types::{FormattingOptions, TextDocumentIdentifier, WorkDoneProgressParams};
        DocumentRangeFormattingParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range,
            options: FormattingOptions::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        }
    }

    #[test]
    fn range_formatting_returns_edits_only_for_changed_lines_in_range() {
        use lsp_types::{Position, Range};
        // Both properties are uniformly over-indented (4 spaces) and would change under a
        // whole-document format; selecting only line 1 must edit line 1 alone — line 2, though it
        // would also change, is out of range and excluded.
        let uri = Uri::from_str("file:///x.otui").expect("uri");
        let text = "Panel\n    id: main\n    width: 10\n";
        let backend = backend_with_doc(&uri, text, Vec::new());

        // A range whose start/end land mid-line still reformats the whole line.
        let range = Range {
            start: Position::new(1, 3),
            end: Position::new(1, 7),
        };
        let edits = backend
            .range_formatting(range_formatting_params(&uri, range))
            .expect("known, parseable document");

        assert_eq!(edits.len(), 1, "got {edits:?}");
        let edit = &edits[0];
        // The whole of original line 1 (col 0 to its end) is replaced with the canonical text.
        assert_eq!(edit.range.start, Position::new(1, 0));
        assert_eq!(
            edit.range.end,
            Position::new(1, "    id: main".len() as u32)
        );
        assert_eq!(edit.new_text, "  id: main");
    }

    #[test]
    fn range_formatting_end_at_next_line_column_zero_excludes_that_line() {
        use lsp_types::{Position, Range};
        // LSP end-exclusive selection: selecting line 1 in full leaves the cursor at the START of
        // line 2 (`{line: 2, character: 0}`). Line 2, though it would also change under a whole-doc
        // format, is NOT part of the selection and must be excluded — only line 1 is edited.
        let uri = Uri::from_str("file:///x.otui").expect("uri");
        let text = "Panel\n    id: main\n    width: 10\n";
        let backend = backend_with_doc(&uri, text, Vec::new());

        let range = Range {
            start: Position::new(1, 0),
            end: Position::new(2, 0),
        };
        let edits = backend
            .range_formatting(range_formatting_params(&uri, range))
            .expect("known, parseable document");

        assert_eq!(edits.len(), 1, "line 2 must be excluded; got {edits:?}");
        assert_eq!(edits[0].range.start, Position::new(1, 0));
        assert_eq!(edits[0].new_text, "  id: main");
    }

    #[test]
    fn range_formatting_on_unparsable_document_is_none() {
        use lsp_types::{Position, Range};
        // Same safety gate as whole-document formatting: an unterminated inline array is an ERROR
        // node, so the engine returns None and the server makes no edit.
        let uri = Uri::from_str("file:///bad.otui").expect("uri");
        let backend = backend_with_doc(&uri, "x: [a, b\n", Vec::new());
        let range = Range {
            start: Position::new(0, 0),
            end: Position::new(0, 8),
        };
        assert!(
            backend
                .range_formatting(range_formatting_params(&uri, range))
                .is_none()
        );
    }

    #[test]
    fn range_formatting_on_unknown_document_is_none() {
        use lsp_types::{Position, Range};
        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        let uri = Uri::from_str("file:///nope.otui").expect("uri");
        let range = Range {
            start: Position::new(0, 0),
            end: Position::new(0, 0),
        };
        assert!(
            backend
                .range_formatting(range_formatting_params(&uri, range))
                .is_none()
        );
    }

    fn on_type_formatting_params(
        uri: &Uri,
        position: lsp_types::Position,
        ch: &str,
    ) -> DocumentOnTypeFormattingParams {
        use lsp_types::{FormattingOptions, TextDocumentIdentifier, TextDocumentPositionParams};
        DocumentOnTypeFormattingParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            ch: ch.to_owned(),
            options: FormattingOptions::default(),
        }
    }

    #[test]
    fn on_type_formatting_after_a_block_opening_line_indents_one_level() {
        use lsp_types::Position;
        // Enter right after "Panel" (a bare container tag, which opens a block): the new, still
        // blank line 1 must be indented two spaces deeper.
        let uri = Uri::from_str("file:///x.otui").expect("uri");
        let text = "Panel\n\n";
        let backend = backend_with_doc(&uri, text, Vec::new());

        let edits = backend
            .on_type_formatting(on_type_formatting_params(&uri, Position::new(1, 0), "\n"))
            .expect("indent_for_line resolves for this document");

        assert_eq!(edits.len(), 1, "got {edits:?}");
        let edit = &edits[0];
        assert_eq!(edit.range.start, Position::new(1, 0));
        assert_eq!(edit.range.end, Position::new(1, 0));
        assert_eq!(edit.new_text, "  ");
    }

    #[test]
    fn on_type_formatting_after_a_plain_property_line_keeps_the_same_indent() {
        use lsp_types::Position;
        // Enter right after "  id: main" (a colon-keyed line with an inline value, a leaf): the new
        // line stays at the same depth, not one deeper.
        let uri = Uri::from_str("file:///x.otui").expect("uri");
        let text = "Panel\n  id: main\n\n";
        let backend = backend_with_doc(&uri, text, Vec::new());

        let edits = backend
            .on_type_formatting(on_type_formatting_params(&uri, Position::new(2, 0), "\n"))
            .expect("indent_for_line resolves for this document");

        assert_eq!(edits.len(), 1, "got {edits:?}");
        let edit = &edits[0];
        assert_eq!(edit.range.start, Position::new(2, 0));
        assert_eq!(edit.range.end, Position::new(2, 0));
        assert_eq!(edit.new_text, "  ");
    }

    #[test]
    fn on_type_formatting_already_at_the_target_indent_is_none() {
        use lsp_types::Position;
        // The new line already carries exactly the two spaces `indent_for_line` would propose:
        // idempotence requires no edit, not a no-op replace.
        let uri = Uri::from_str("file:///x.otui").expect("uri");
        let text = "Panel\n  id: main\n  ";
        let backend = backend_with_doc(&uri, text, Vec::new());

        assert!(
            backend
                .on_type_formatting(on_type_formatting_params(&uri, Position::new(2, 2), "\n"))
                .is_none()
        );
    }

    #[test]
    fn on_type_formatting_ignores_a_non_newline_trigger_character() {
        use lsp_types::Position;
        // Only "\n" is registered as a trigger character; anything else must be a no-op even though
        // the line itself would otherwise need reindenting.
        let uri = Uri::from_str("file:///x.otui").expect("uri");
        let text = "Panel\n\n";
        let backend = backend_with_doc(&uri, text, Vec::new());

        assert!(
            backend
                .on_type_formatting(on_type_formatting_params(&uri, Position::new(1, 0), "}"))
                .is_none()
        );
    }

    #[test]
    fn on_type_formatting_inside_a_block_scalar_body_is_none() {
        use lsp_types::Position;
        // `indent_for_line` refuses to guess inside an open block-scalar body (raw Lua content);
        // the handler must pass that refusal straight through rather than substitute a guess.
        let uri = Uri::from_str("file:///x.otui").expect("uri");
        let text = "Panel\n  @onClick: |\n    self:hide()\n";
        let backend = backend_with_doc(&uri, text, Vec::new());

        assert!(
            backend
                .on_type_formatting(on_type_formatting_params(&uri, Position::new(2, 4), "\n"))
                .is_none()
        );
    }

    #[test]
    fn on_type_formatting_on_unknown_document_is_none() {
        use lsp_types::Position;
        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        let uri = Uri::from_str("file:///nope.otui").expect("uri");
        assert!(
            backend
                .on_type_formatting(on_type_formatting_params(&uri, Position::new(0, 0), "\n"))
                .is_none()
        );
    }

    // --- module-association index (`ModuleUiIndex`/`scan_module_dir`/`find_owning_module_dir`) ---
    //
    // Focused, disk-backed unit tests of the pure resolution logic, one directory tree per test — the
    // full request/response coverage (driving `textDocument/references` through the real LSP loop in
    // both directions) lives in `tests/loop_dispatch.rs`; these pin the underlying disk seam the way
    // `scan_workspace_indexes_otui_and_skips_binary_and_non_otui` pins `scan_workspace`'s.

    /// RAII guard mirroring `tests/loop_dispatch.rs`'s `TempDirGuard`: removes its directory on drop,
    /// including on an unwinding panic from a failed assertion (unlike a trailing
    /// `std::fs::remove_dir_all`, which a panic skips and leaks the temp directory).
    struct ModuleTestDir(std::path::PathBuf);
    impl Drop for ModuleTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scan_module_dir_resolves_a_controller_and_ui_file_across_directories() {
        // The exact shape the fast-path `paired_uri` alone cannot resolve: `ctrl.lua` and
        // `styles/ui.otui` share neither a directory nor a stem.
        let base = std::env::temp_dir().join(format!("otui-module-scan-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        std::fs::create_dir_all(base.join("styles")).expect("mkdir styles");
        std::fs::write(
            base.join("mymodule.otmod"),
            "Module\n  name: mymodule\n  scripts: [ ctrl ]\n",
        )
        .expect("write otmod");
        std::fs::write(
            base.join("ctrl.lua"),
            "function onCreate(w)\n  g_ui.loadUI('styles/ui')\n  \
             local btn = w:getChildById('x')\nend\n",
        )
        .expect("write ctrl.lua");
        std::fs::write(
            base.join("styles").join("ui.otui"),
            "MainWindow < UIWidget\n  Button\n    id: x\n",
        )
        .expect("write ui.otui");

        let pairs = scan_module_dir(&base, &[]);
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert!(pairs[0].0.as_str().ends_with("ctrl.lua"), "{pairs:?}");
        assert!(
            pairs[0].1.as_str().ends_with("styles/ui.otui")
                || pairs[0].1.as_str().ends_with("styles%2Fui.otui"),
            "{pairs:?}"
        );
    }

    #[test]
    fn scan_module_dir_ignores_a_load_ui_target_that_does_not_exist_on_disk() {
        // Best-effort / no-false-pairing rule: a `loadUI` argument naming a file that is not actually
        // on disk must never become a pair.
        let base =
            std::env::temp_dir().join(format!("otui-module-scan-missing-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(base.join("m.otmod"), "Module\n  scripts: [ ctrl ]\n").expect("write otmod");
        std::fs::write(base.join("ctrl.lua"), "g_ui.loadUI('does-not-exist')\n")
            .expect("write ctrl.lua");

        assert!(scan_module_dir(&base, &[]).is_empty());
    }

    #[test]
    fn scan_module_dir_ignores_a_variable_load_ui_argument() {
        let base =
            std::env::temp_dir().join(format!("otui-module-scan-var-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(base.join("m.otmod"), "Module\n  scripts: [ ctrl ]\n").expect("write otmod");
        std::fs::write(base.join("ctrl.lua"), "g_ui.loadUI(dynamicName)\n")
            .expect("write ctrl.lua");
        std::fs::write(base.join("dynamicName.otui"), "Panel < UIWidget\n").expect("write otui");

        assert!(
            scan_module_dir(&base, &[]).is_empty(),
            "a variable argument must never be resolved, even if a file happens to share its name"
        );
    }

    #[test]
    fn scan_module_dir_ignores_a_load_ui_argument_that_walks_up_with_dotdot() {
        // A complete-literal `g_ui.loadUI('../otherModule/ui')` must never pair, even when the
        // target genuinely exists on disk in the foreign module directory — the engine's own
        // `resolvePath` does not collapse `..`, and PhysFS rejects a path containing one, so the
        // real engine would never load this target. Resolving it here via a plain `Path::join` +
        // `is_file()` check (pre-fix behavior) would produce exactly the false controller<->UI
        // pairing this whole mechanism exists to prevent.
        let base =
            std::env::temp_dir().join(format!("otui-module-scan-dotdot-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        let module_dir = base.join("moduleA");
        let other_dir = base.join("otherModule");
        std::fs::create_dir_all(&module_dir).expect("mkdir moduleA");
        std::fs::create_dir_all(&other_dir).expect("mkdir otherModule");
        std::fs::write(
            module_dir.join("moduleA.otmod"),
            "Module\n  scripts: [ ctrl ]\n",
        )
        .expect("write otmod");
        std::fs::write(
            module_dir.join("ctrl.lua"),
            "g_ui.loadUI('../otherModule/ui')\n",
        )
        .expect("write ctrl.lua");
        std::fs::write(other_dir.join("ui.otui"), "Panel < UIWidget\n").expect("write ui.otui");

        assert!(
            scan_module_dir(&module_dir, &[]).is_empty(),
            "a `..`-walking argument must never be resolved, even if the target exists on disk"
        );
    }

    #[test]
    fn scan_module_dir_resolves_a_wildcard_scripts_entry() {
        // `scripts: [*]` (module doc comment / `otmod_scripts`'s doc comment): every `.lua` directly
        // reachable under the module directory, recursively. The `loadUI`-style target resolves
        // against the CONTROLLER's own directory (`nested/`), not the module root — see
        // `scan_module_dir`'s doc comment (`getCurrentSourcePath` / the `bestiary.lua` corpus
        // example) — so `deep.otui` must sit next to `deep.lua`, not at the module root.
        let base =
            std::env::temp_dir().join(format!("otui-module-scan-wild-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        std::fs::create_dir_all(base.join("nested")).expect("mkdir nested");
        std::fs::write(base.join("m.otmod"), "Module\n  scripts: [*]\n").expect("write otmod");
        std::fs::write(
            base.join("nested").join("deep.lua"),
            "g_ui.importStyle('deep')\n",
        )
        .expect("write deep.lua");
        std::fs::write(base.join("nested").join("deep.otui"), "Panel < UIWidget\n")
            .expect("write otui");

        let pairs = scan_module_dir(&base, &[]);
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert!(pairs[0].0.as_str().ends_with("deep.lua"), "{pairs:?}");
        assert!(
            pairs[0].1.as_str().ends_with("nested/deep.otui"),
            "{pairs:?}"
        );
    }

    #[test]
    fn scan_module_dir_resolves_a_nested_controllers_ui_relative_to_its_own_directory() {
        // The exact real-corpus shape (`game_cyclopedia/tab/bestiary/bestiary.lua`): a controller
        // named by `scripts:` lives in a SUBdirectory of the module, and its `loadUI` target sits
        // next to it, not at the module root.
        let base =
            std::env::temp_dir().join(format!("otui-module-scan-nested-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        std::fs::create_dir_all(base.join("tab").join("bestiary")).expect("mkdir");
        std::fs::write(
            base.join("m.otmod"),
            "Module\n  scripts: [ tab/bestiary/bestiary ]\n",
        )
        .expect("write otmod");
        std::fs::write(
            base.join("tab").join("bestiary").join("bestiary.lua"),
            "UI = g_ui.loadUI(\"bestiary\", contentContainer)\n",
        )
        .expect("write bestiary.lua");
        std::fs::write(
            base.join("tab").join("bestiary").join("bestiary.otui"),
            "Panel < UIWidget\n",
        )
        .expect("write bestiary.otui");
        // A decoy at the module root must NOT be what resolves — proves the fix actually matters
        // (resolving against `module_dir` would have found this file instead and passed anyway).
        std::fs::write(base.join("bestiary.otui"), "Panel < UIWidget\n  // decoy\n")
            .expect("decoy");

        let pairs = scan_module_dir(&base, &[]);
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert!(
            pairs[0].1.as_str().ends_with("tab/bestiary/bestiary.otui"),
            "must resolve next to the controller, not at the module root: {pairs:?}"
        );
    }

    #[test]
    fn scan_module_dir_resolves_a_vfs_rooted_load_ui_path_against_the_client_root() {
        // A `/`-rooted `loadUI` argument is a VFS-absolute path (see `resolve_vfs_rooted_otui`'s doc
        // comment) — resolved against the client root's `modules/` overlay, into a DIFFERENT
        // module's directory than the controller itself, never a plain join onto the controller's
        // own directory (which would look for a nonexistent `mymodule/othermod/styles/ui.otui`).
        let base =
            std::env::temp_dir().join(format!("otui-module-scan-vfs-root-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        let my_module_dir = base.join("modules").join("mymodule");
        std::fs::create_dir_all(&my_module_dir).expect("mkdir mymodule");
        std::fs::write(
            my_module_dir.join("m.otmod"),
            "Module\n  scripts: [ ctrl ]\n",
        )
        .expect("write otmod");
        std::fs::write(
            my_module_dir.join("ctrl.lua"),
            "g_ui.loadUI('/modules/othermod/styles/ui')\n",
        )
        .expect("write ctrl.lua");
        let other_styles_dir = base.join("modules").join("othermod").join("styles");
        std::fs::create_dir_all(&other_styles_dir).expect("mkdir othermod/styles");
        std::fs::write(other_styles_dir.join("ui.otui"), "Panel < UIWidget\n").expect("write otui");

        // Without a client root: silence, never a guess.
        assert!(
            scan_module_dir(&my_module_dir, &[]).is_empty(),
            "a /-rooted path must never resolve without a detected client root"
        );

        // With the client root: resolves into the OTHER module's directory.
        let pairs = scan_module_dir(&my_module_dir, std::slice::from_ref(&base));
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert!(pairs[0].0.as_str().ends_with("ctrl.lua"), "{pairs:?}");
        assert!(
            pairs[0].1.as_str().ends_with("othermod/styles/ui.otui")
                || pairs[0].1.as_str().ends_with("othermod%2Fstyles%2Fui.otui"),
            "{pairs:?}"
        );
    }

    #[test]
    fn resolve_vfs_rooted_otui_finds_a_target_under_the_modules_overlay() {
        let base =
            std::env::temp_dir().join(format!("otui-resolve-vfs-rooted-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        let dir = base.join("modules").join("game_npctrade").join("templates");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("npctrade_legacy.otui"), "Panel < UIWidget\n").expect("write otui");

        let found = resolve_vfs_rooted_otui(
            "game_npctrade/templates/npctrade_legacy",
            std::slice::from_ref(&base),
        );
        assert_eq!(
            found,
            Some(dir.join("npctrade_legacy.otui")),
            "extensionless rest implies .otui, resolved via the modules/ overlay"
        );

        assert_eq!(
            resolve_vfs_rooted_otui("does/not/exist", std::slice::from_ref(&base)),
            None
        );
    }

    #[test]
    fn scan_module_dir_is_empty_when_there_is_no_otmod() {
        let base =
            std::env::temp_dir().join(format!("otui-module-scan-none-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(base.join("ctrl.lua"), "g_ui.loadUI('x')\n").expect("write ctrl.lua");
        std::fs::write(base.join("x.otui"), "Panel < UIWidget\n").expect("write otui");

        assert!(scan_module_dir(&base, &[]).is_empty());
    }

    #[test]
    fn find_module_dirs_finds_a_nested_module_directory() {
        let base = std::env::temp_dir().join(format!("otui-find-modules-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        let module_dir = base.join("modules").join("nested_module");
        std::fs::create_dir_all(&module_dir).expect("mkdir");
        std::fs::write(module_dir.join("nested_module.otmod"), "Module\n").expect("write otmod");
        std::fs::create_dir_all(base.join("modules").join("no_otmod_here")).expect("mkdir");

        let root = uri_from_file_path(&base).expect("root uri");
        let dirs = find_module_dirs(&[root]);
        assert_eq!(dirs, vec![module_dir]);
    }

    #[test]
    fn find_module_dirs_matches_an_uppercase_otmod_extension() {
        // `is_otmod_uri`/`dir_has_otmod`/`scan_module_dir` all match `.otmod` case-insensitively —
        // the initial discovery walk (`collect_module_dirs`) must too, or a `.OTMOD` module would be
        // invisible until a watcher event fires for it.
        let base = std::env::temp_dir().join(format!(
            "otui-find-modules-uppercase-{}",
            std::process::id()
        ));
        let _cleanup = ModuleTestDir(base.clone());
        let module_dir = base.join("modules").join("upper_module");
        std::fs::create_dir_all(&module_dir).expect("mkdir");
        std::fs::write(module_dir.join("upper_module.OTMOD"), "Module\n").expect("write otmod");

        let root = uri_from_file_path(&base).expect("root uri");
        let dirs = find_module_dirs(&[root]);
        assert_eq!(dirs, vec![module_dir]);
    }

    #[test]
    fn find_owning_module_dir_walks_up_to_the_nearest_otmod() {
        let base = std::env::temp_dir().join(format!("otui-owning-module-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        let module_dir = base.join("game_wheel");
        let deep = module_dir.join("classes");
        std::fs::create_dir_all(&deep).expect("mkdir");
        std::fs::write(module_dir.join("wheel.otmod"), "Module\n").expect("write otmod");

        let found = find_owning_module_dir(&deep, std::slice::from_ref(&base));
        assert_eq!(found, Some(module_dir));
    }

    #[test]
    fn find_owning_module_dir_stops_at_a_workspace_root_and_finds_nothing_beyond_it() {
        // A file living outside any module (most of `data/`, for instance) must not walk past the
        // workspace root looking for a module that owns it, and must not find an unrelated module
        // sitting further up an unbounded tree.
        let base =
            std::env::temp_dir().join(format!("otui-owning-module-bound-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        // An otmod ABOVE the workspace root must never be found.
        std::fs::write(
            std::env::temp_dir().join(format!("unrelated-{}.otmod", std::process::id())),
            "Module\n",
        )
        .ok();
        let data_dir = base.join("data").join("styles");
        std::fs::create_dir_all(&data_dir).expect("mkdir");

        let found = find_owning_module_dir(&data_dir, std::slice::from_ref(&base));
        assert!(found.is_none(), "{found:?}");
        // Cleanup the stray file created above (best-effort; a leaked temp file elsewhere costs
        // nothing but tidiness).
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("unrelated-{}.otmod", std::process::id())),
        );
    }

    #[test]
    fn build_module_index_aggregates_every_module_and_supports_bidirectional_lookup() {
        let base =
            std::env::temp_dir().join(format!("otui-build-module-index-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        let module_dir = base.join("mymodule");
        std::fs::create_dir_all(module_dir.join("styles")).expect("mkdir");
        std::fs::write(module_dir.join("m.otmod"), "Module\n  scripts: [ ctrl ]\n").expect("otmod");
        std::fs::write(module_dir.join("ctrl.lua"), "g_ui.loadUI('styles/ui')\n")
            .expect("ctrl.lua");
        std::fs::write(
            module_dir.join("styles").join("ui.otui"),
            "Panel < UIWidget\n",
        )
        .expect("otui");

        let root = uri_from_file_path(&base).expect("root uri");
        let index = build_module_index(&[root]);
        assert_eq!(index.module_count(), 1);

        let lua_uri = uri_from_file_path(&module_dir.join("ctrl.lua")).expect("uri");
        let otui_uri = uri_from_file_path(&module_dir.join("styles").join("ui.otui")).expect("uri");
        assert_eq!(
            index.otui_files_for(&lua_uri),
            std::slice::from_ref(&otui_uri)
        );
        assert_eq!(index.lua_files_for(&otui_uri), [lua_uri]);
    }

    #[test]
    fn module_ui_index_set_module_with_empty_pairs_removes_the_module() {
        let mut index = ModuleUiIndex::new();
        let dir = std::path::PathBuf::from("/scratch/mymodule");
        let lua = Uri::from_str("file:///scratch/mymodule/ctrl.lua").expect("uri");
        let otui = Uri::from_str("file:///scratch/mymodule/styles/ui.otui").expect("uri");
        index.set_module(dir.clone(), vec![(lua.clone(), otui.clone())]);
        assert_eq!(index.module_count(), 1);
        assert_eq!(index.otui_files_for(&lua), std::slice::from_ref(&otui));

        index.set_module(dir, Vec::new());
        assert_eq!(index.module_count(), 0);
        assert!(index.otui_files_for(&lua).is_empty());
        assert!(index.lua_files_for(&otui).is_empty());
    }

    #[test]
    fn watched_otmod_creation_populates_the_module_index_without_a_workspace_scan() {
        use lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let base = std::env::temp_dir().join(format!("otui-watch-otmod-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        std::fs::create_dir_all(base.join("styles")).expect("mkdir");
        let ctrl_path = base.join("ctrl.lua");
        std::fs::write(&ctrl_path, "g_ui.loadUI('styles/ui')\n").expect("write ctrl.lua");
        std::fs::write(base.join("styles").join("ui.otui"), "Panel < UIWidget\n")
            .expect("write ui.otui");
        let otmod_path = base.join("m.otmod");
        std::fs::write(&otmod_path, "Module\n  scripts: [ ctrl ]\n").expect("write otmod");

        // No workspace root registered at all — proves this update is driven purely by the watched
        // file event handler, never by the (in this case nonexistent) background scan.
        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());

        let ctrl_uri = uri_from_file_path(&ctrl_path).expect("uri");
        let ui_uri = uri_from_file_path(&base.join("styles").join("ui.otui")).expect("uri");
        let otmod_uri = uri_from_file_path(&otmod_path).expect("uri");
        // `associated_uris` always includes `paired_uri`'s same-stem candidate whether or not it
        // exists on disk (existence is checked downstream, by whoever tries to read its text) — so
        // "nothing indexed yet" is checked by absence of the module-derived `ui_uri`, not emptiness.
        assert!(
            !backend.associated_uris(&ctrl_uri, "otui").contains(&ui_uri),
            "the module association must not exist before the watched change fires"
        );

        backend.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: otmod_uri.clone(),
                typ: FileChangeType::CREATED,
            }],
        });

        assert!(backend.associated_uris(&ctrl_uri, "otui").contains(&ui_uri));
        assert!(backend.associated_uris(&ui_uri, "lua").contains(&ctrl_uri));
        // A `.otmod` manifest must never be routed into the OTUI style index — it is not a real
        // `.otui` document (see `is_otmod_uri`'s doc comment).
        assert!(
            backend
                .style_index
                .read()
                .expect("style_index lock poisoned")
                .document(&DocId::from(otmod_uri.to_string()))
                .is_none()
        );
    }

    #[test]
    fn watched_otmod_deletion_clears_its_modules_associations() {
        use lsp_types::{DidChangeWatchedFilesParams, FileChangeType, FileEvent};

        let base =
            std::env::temp_dir().join(format!("otui-watch-otmod-del-{}", std::process::id()));
        let _cleanup = ModuleTestDir(base.clone());
        std::fs::create_dir_all(&base).expect("mkdir");
        std::fs::write(base.join("ctrl.lua"), "g_ui.loadUI('ui')\n").expect("write ctrl.lua");
        std::fs::write(base.join("ui.otui"), "Panel < UIWidget\n").expect("write ui.otui");
        let otmod_path = base.join("m.otmod");
        std::fs::write(&otmod_path, "Module\n  scripts: [ ctrl ]\n").expect("write otmod");

        let (tx, _rx) = crossbeam_channel::unbounded();
        let backend = Backend::new(tx, &InitializeParams::default());
        let otmod_uri = uri_from_file_path(&otmod_path).expect("uri");
        let ctrl_uri = uri_from_file_path(&base.join("ctrl.lua")).expect("uri");
        let ui_uri = uri_from_file_path(&base.join("ui.otui")).expect("uri");

        backend.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: otmod_uri.clone(),
                typ: FileChangeType::CREATED,
            }],
        });
        assert!(backend.associated_uris(&ctrl_uri, "otui").contains(&ui_uri));

        // Now the `.otmod` is deleted on disk, and the watcher tells us.
        std::fs::remove_file(&otmod_path).expect("remove otmod");
        backend.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: otmod_uri,
                typ: FileChangeType::DELETED,
            }],
        });
        assert!(
            !backend.associated_uris(&ctrl_uri, "otui").contains(&ui_uri),
            "a deleted .otmod must clear its module's associations"
        );
    }
}
