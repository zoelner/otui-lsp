//! Module manifest schema + diagnostics.
//!
//! A `.otmod` file is OTClient's module manifest. It parses through the **identical** OTML grammar
//! every widget `.otui` style sheet does (see [`crate::otmod`]'s doc comment for the full case), but
//! its top-level keys belong to a completely different, much smaller vocabulary — manifest
//! metadata (`name`, `description`, `scripts`, …) — never a widget style property. Running the
//! widget-aware diagnostics ([`crate::diagnostics::analyze_with_widgets`]) over a manifest sprays
//! every one of its keys with a spurious `unknown-property` hint: `name:`/`description:`/`scripts:`
//! are not widget properties, and the widget catalog was never meant to judge them.
//!
//! [`analyze_manifest`] is the manifest-flavored counterpart: the same schema-agnostic structural
//! passes every OTML document shares (tab/odd indentation, invalid depth, syntax errors — see
//! [`crate::diagnostics::structural_diagnostics`]), plus two manifest-specific checks derived
//! straight from the engine's own manifest reader instead of the widget catalog.
//!
//! ## The manifest key set (ground truth: `module.cpp` / `modulemanager.cpp`)
//!
//! `ModuleManager::discoverModule` (`src/framework/core/modulemanager.cpp:74-83`) requires a
//! top-level node tagged exactly `Module` (`doc->at("Module")`) — `OTMLNode::at` **throws** an
//! `OTMLException` when no such child exists (`otmlnode.cpp:68-77`), and that exception is caught
//! only by `discoverModule`'s own `try`/`catch` (`g_logger.error(...)`, function returns `nullptr`):
//! the module is never registered — see [`MISSING_MODULE_ROOT`]. It then reads the module's `name`
//! the same throwing way (`moduleNode->valueAt("name")`, no default overload).
//!
//! `Module::discover` (`src/framework/core/module.cpp:210-274`) reads exactly these further keys
//! off that node, each via `valueAt`/`get` — both of which only ever inspect the node's own
//! **direct** children, never recursing (`OTMLNode::get`, `otmlnode.cpp:54-61`):
//!
//! * `description`, `author`, `website`, `version` — `valueAt(key, "none")` (a string default; any
//!   value including an empty one is accepted, so this is never a source of a diagnostic);
//! * `enabled`, `autoload`, `reloadable`, `sandboxed` — `valueAt<bool>(key, default)`;
//! * `autoload-priority` — `valueAt<int>(key, 9999)`;
//! * `devices`, `dependencies`, `scripts`, `load-later` — `get(key)`, then iterated as a list;
//! * `@onLoad`, `@onUnload` — `get(key)`, the two event properties (the OTML tag for these lines is
//!   the **whole** `@onLoad`/`@onUnload` text, `@` included — `OTMLParser::parseNode` never strips a
//!   leading `@`, unlike the tree-sitter grammar's aliased `event_name` node, which spans just the
//!   bare name after the `@`; [`manifest_key_of`] re-adds the `@` before comparing).
//!
//! `name` itself is also read again inside `Module::discover`'s caller context, but not by
//! `discover` — it is listed as a known key here anyway since a manifest legitimately carries it.
//!
//! Sixteen keys in total. Every one of the 82 `.otmod` files in the real OTClient module corpus
//! uses only keys from this set (`cargo xtask corpus`'s manifest census — see the doc comment on
//! [`analyze_manifest`]) with a single exception: four files (`game_inspect`, `game_proficiency`,
//! `game_tutorial`, `game_taskboard`) carry a `minClientVersion:` key that appears **nowhere** in the
//! engine's C++ source — `Module::discover` never reads it, so it is silently ignored exactly like
//! any other unrecognized key, a genuine (if harmless) real-world [`UNKNOWN_MANIFEST_KEY`] hit this
//! diagnostic is designed to surface.
//!
//! ## The `.otfont` font manifest (ground truth: `fontmanager.cpp` / `bitmapfont.cpp`)
//!
//! `.otfont` is a second, unrelated OTML manifest schema (a *bitmap* font description, not a widget
//! or a module): `FontManager::importFont` (`src/framework/graphics/fontmanager.cpp:53-88`) requires
//! a top-level node tagged exactly `Font` (`doc->at("Font")`, throwing exactly like
//! `doc->at("Module")` does — see [`MISSING_FONT_ROOT`]), reads its `name` the same throwing way,
//! then hands the node to `BitmapFont::load` (`src/framework/graphics/bitmapfont.cpp:36-81`), which
//! reads: `texture` (`at`, throws if absent), `glyph-size` (`valueAt<Size>`, no default, throws),
//! `height` (`valueAt<int>`, no default, throws), `space-width`/`y-offset`/`first-glyph`/`spacing`
//! (each `valueAt` with a default) and `fixed-glyph-width` (`get`, optional). `default` and
//! `widget-default` are read back in `FontManager::importFont` itself
//! (`valueAt<bool>(key, false)`). Eleven keys in total. Of the 36 `.otfont` files in the real
//! corpus, every key used is in this set with one exception: one file (`verdana-10px-rounded`)
//! carries an `x-offset:` key that `BitmapFont::load` never reads (only `y-offset` is) — the same
//! kind of harmless real-world unknown-key hit `minClientVersion` is for `.otmod`.
use crate::diagnostics::structural_diagnostics;
use crate::syntax::SyntaxTree;
use lang_api::{ByteSpan, Diagnostic, Severity};
use tree_sitter::Node;

/// Every ordinary `key:` the engine reads directly off a `Module` node (spec: `module.cpp`'s
/// `Module::discover`, plus `name` from `modulemanager.cpp`'s `discoverModule` — see this module's
/// doc comment for the full citation). Compared **case-sensitively**: `OTMLNode::tag()` is compared
/// with plain `==`, and nothing in `Module::discover` lowercases a key before looking it up.
const KNOWN_MANIFEST_KEYS: &[&str] = &[
    "name",
    "description",
    "author",
    "website",
    "version",
    "enabled",
    "autoload",
    "reloadable",
    "sandboxed",
    "autoload-priority",
    "devices",
    "dependencies",
    "scripts",
    "load-later",
];

/// The two `@event:` properties `Module::discover` reads (`@onLoad`, `@onUnload`), spelled without
/// their leading `@` — the tree-sitter grammar's `event_property` rule aliases only the part after
/// `@` to its `key` field (see [`manifest_key_of`]), so the `@` is re-added by the caller before
/// comparing against [`KNOWN_MANIFEST_KEYS`].
const KNOWN_MANIFEST_EVENTS: &[&str] = &["onLoad", "onUnload"];

/// Every key `FontManager::importFont`/`BitmapFont::load` reads directly off a `Font` node (see
/// this module's doc comment for the full citation). No `@event:` keys — a font manifest has none.
const KNOWN_FONT_KEYS: &[&str] = &[
    "name",
    "texture",
    "glyph-size",
    "height",
    "space-width",
    "y-offset",
    "first-glyph",
    "spacing",
    "fixed-glyph-width",
    "default",
    "widget-default",
];

/// Diagnostic code: the document has no top-level node tagged `Module`. Severity
/// [`Severity::Error`]: `ModuleManager::discoverModule`'s `doc->at("Module")` **throws** when no
/// such child exists (see this module's doc comment); the exception is caught by that function's
/// own `try`/`catch`, logged, and the module is never registered — the manifest fails to load as a
/// module, exactly the "file fails to load" severity class `unknown-root-style` already uses for
/// the widget side (see [`crate::diagnostics::UNKNOWN_ROOT_STYLE`]).
pub const MISSING_MODULE_ROOT: &str = "missing-module-root";

/// Diagnostic code: a key under the manifest's `Module` node that is not one of the ~16 keys
/// [`Module::discover`](self) actually reads. Severity [`Severity::Hint`] (spec §2.10's posture,
/// mirroring [`crate::diagnostics::UNKNOWN_PROPERTY`]): `OTMLNode::get`/`valueAt` simply never finds
/// the key, so it has no effect whatsoever — never a load failure, never coerced, just silently
/// unread. Shared with the font schema ([`analyze_font_manifest`]) — the wording is schema-neutral
/// ("manifest", not "module"), and the two never fire on the same document.
pub const UNKNOWN_MANIFEST_KEY: &str = "unknown-manifest-key";

/// Diagnostic code: the document has no top-level node tagged `Font`. Severity [`Severity::Error`],
/// for exactly the same reason as [`MISSING_MODULE_ROOT`]: `FontManager::importFont`'s
/// `doc->at("Font")` throws without one, caught only by that function's own `try`/`catch`
/// (`g_logger.error(...)`, returns `false`) — the font is never loaded.
pub const MISSING_FONT_ROOT: &str = "missing-font-root";

/// One manifest schema: the root tag the engine requires, the ordinary keys it reads off that
/// root, and (module manifests only) the `@event:` keys — everything [`analyze_with_schema`] needs
/// to run the two schema-specific checks against a parsed document.
struct Schema {
    /// The bare tag the root node must carry (`OTMLNode::at`'s exact-match lookup).
    root_tag: &'static str,
    /// Ordinary `key:` names the engine reads directly off the root node.
    keys: &'static [&'static str],
    /// `@event:` names (without the leading `@`) the engine reads; empty for a schema with none.
    events: &'static [&'static str],
    /// The diagnostic code for "no top-level node tagged `root_tag`".
    missing_root_code: &'static str,
    /// The diagnostic message for that same finding.
    missing_root_message: &'static str,
}

/// The `.otmod` module-manifest schema (`module.cpp`/`modulemanager.cpp` — see this module's doc
/// comment).
const MODULE_SCHEMA: Schema = Schema {
    root_tag: "Module",
    keys: KNOWN_MANIFEST_KEYS,
    events: KNOWN_MANIFEST_EVENTS,
    missing_root_code: MISSING_MODULE_ROOT,
    missing_root_message: "no top-level `Module` node: `ModuleManager::discoverModule` requires \
                            one and throws without it, so this manifest fails to load as a module",
};

/// The `.otfont` font-manifest schema (`fontmanager.cpp`/`bitmapfont.cpp` — see this module's doc
/// comment).
const FONT_SCHEMA: Schema = Schema {
    root_tag: "Font",
    keys: KNOWN_FONT_KEYS,
    events: &[],
    missing_root_code: MISSING_FONT_ROOT,
    missing_root_message: "no top-level `Font` node: `FontManager::importFont` requires one and \
                            throws without it, so this manifest fails to load as a font",
};

/// Compute manifest diagnostics for a `.otmod` document: the schema-agnostic structural passes
/// every OTML document shares ([`structural_diagnostics`]) plus the two manifest-specific checks
/// above. Deliberately does **not** run any widget check (unknown-property against the widget
/// catalog, anchors, `$state`, style-base resolution, …) — those all assume a widget tree, and a
/// module manifest is not one.
///
/// Returns findings sorted by span (`start`, then `end`), matching
/// [`crate::diagnostics::analyze`]'s contract.
#[must_use]
pub fn analyze_manifest(source: &str) -> Vec<Diagnostic> {
    analyze_with_schema(source, &MODULE_SCHEMA)
}

/// Like [`analyze_manifest`], but for a `.otfont` font manifest (see this module's doc comment for
/// the schema). Same structural passes, same "unknown key is a hint, missing root is an error"
/// shape, judged against the font schema instead of the module one.
#[must_use]
pub fn analyze_font_manifest(source: &str) -> Vec<Diagnostic> {
    analyze_with_schema(source, &FONT_SCHEMA)
}

/// The shared engine behind [`analyze_manifest`]/[`analyze_font_manifest`]: the schema-agnostic
/// structural passes, plus [`check_root_and_keys`] judged against `schema`.
fn analyze_with_schema(source: &str, schema: &Schema) -> Vec<Diagnostic> {
    let tree = SyntaxTree::parse(source);
    let mut out = structural_diagnostics(source, tree.as_ref());
    if let Some(tree) = &tree {
        check_root_and_keys(tree.root(), source, schema, &mut out);
    }
    out.sort_by_key(|d| (d.span.start, d.span.end));
    out
}

/// Find the document's top-level node tagged `schema.root_tag` (mirroring `OTMLNode::at`'s "first
/// non-null child with this exact tag" rule) and, if found, walk its direct children for unknown
/// keys. A missing root is `schema.missing_root_code` and short-circuits the key check entirely:
/// with no root node there is nothing to walk, and the engine never gets far enough to look at any
/// key anyway.
fn check_root_and_keys(root: Node<'_>, source: &str, schema: &Schema, out: &mut Vec<Diagnostic>) {
    let mut cursor = root.walk();
    let manifest_root = root
        .named_children(&mut cursor)
        .find(|child| child.kind() == "container" && tag_text(*child, source) == schema.root_tag);

    let Some(manifest_root) = manifest_root else {
        out.push(Diagnostic {
            severity: Severity::Error,
            code: schema.missing_root_code,
            message: schema.missing_root_message.to_owned(),
            span: root_span(root, source),
        });
        return;
    };

    let mut cursor = manifest_root.walk();
    for child in manifest_root.named_children(&mut cursor) {
        let Some((key, key_span)) = manifest_key_of(child, source) else {
            continue;
        };
        if is_known_manifest_key(key, schema) {
            continue;
        }
        out.push(Diagnostic {
            severity: Severity::Hint,
            code: UNKNOWN_MANIFEST_KEY,
            message: format!(
                "unknown manifest key `{key}`: the engine never reads it, so it has no effect"
            ),
            span: key_span,
        });
    }
}

/// Whether `key` (as returned by [`manifest_key_of`]) is one `schema` actually reads.
fn is_known_manifest_key(key: &str, schema: &Schema) -> bool {
    if let Some(event) = key.strip_prefix('@') {
        return schema.events.contains(&event);
    }
    schema.keys.contains(&key)
}

/// The manifest-relevant "key" text + span for one of `Module`'s direct children, or `None` for a
/// child kind that carries no comparable key at all (a nested bare-tag container, a `$state`
/// selector, …) — none of which appear anywhere in the real corpus, and none of which the engine
/// reads either way, so skipping them costs no coverage.
fn manifest_key_of<'a>(node: Node<'_>, source: &'a str) -> Option<(&'a str, ByteSpan)> {
    match node.kind() {
        "property" => {
            let key = node.child_by_field_name("key")?;
            Some((slice(source, key), SyntaxTree::span_of(key)))
        }
        // The grammar's `event_property` rule is `seq(field('key', seq('@', alias(...,
        // $.event_name))), ':', ...)`: the `key` field is attached to *both* the anonymous `@`
        // token and the aliased `event_name` node (they are both productions of the same
        // `field()`-wrapped `seq`), and `child_by_field_name` returns the FIRST field-tagged
        // child — the anonymous `@`, not `event_name`. Locating the named `event_name` child
        // directly (rather than trusting the field) sidesteps that ambiguity; verified against
        // this crate's own parser at runtime, not just the grammar source.
        "event_property" => {
            let mut cursor = node.walk();
            let name = node
                .named_children(&mut cursor)
                .find(|c| c.kind() == "event_name")?;
            // Widen the span by one byte to the left to include the literal `@` that
            // `token.immediate` guarantees sits directly before the aliased `event_name` (no
            // space possible between them in this grammar), then slice `source` over that
            // combined span — `@onLoad`, not just `onLoad` — so the returned text and span both
            // cover exactly what the engine's own `OTMLNode::tag()` holds for this line.
            let name_span = SyntaxTree::span_of(name);
            let at_span = ByteSpan::new(name_span.start.saturating_sub(1), name_span.end);
            if source.as_bytes().get(at_span.start) == Some(&b'@') {
                Some((&source[at_span.start..at_span.end], at_span))
            } else {
                // Defensive fallback (should be unreachable for a well-formed `event_property`):
                // report the bare name rather than risk slicing a wrong byte.
                Some((slice(source, name), name_span))
            }
        }
        _ => None,
    }
}

/// The bare tag text of a `container` node's `tag` field, trimmed — the external scanner's `tag`
/// token spans the whole line, so callers must trim before comparing (mirroring
/// `crate::diagnostics`'s `slice(..).trim()` pattern for the same field).
fn tag_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    node.child_by_field_name("tag")
        .map(|t| slice(source, t).trim())
        .unwrap_or_default()
}

/// The exact source text a node spans.
fn slice<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

/// The span reported for [`MISSING_MODULE_ROOT`]: the first top-level statement's span (so the
/// diagnostic points at *something* the author wrote), or the whole (empty) document when there is
/// none at all.
fn root_span(root: Node<'_>, source: &str) -> ByteSpan {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .next()
        .map_or_else(|| ByteSpan::new(0, source.len()), SyntaxTree::span_of)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code).collect()
    }

    #[test]
    fn a_well_formed_manifest_has_no_diagnostics() {
        let src = "\
Module
  name: game_shop
  description: In-game shop
  author: someone
  website: https://example.invalid
  version: 1.0
  enabled: true
  autoload: false
  reloadable: true
  sandboxed: true
  autoload-priority: 100
  dependencies: [ game_things ]
  scripts: [ game_shop ]
  load-later: [ game_bosstiary ]
  @onLoad: init()
  @onUnload: terminate()
";
        let diags = analyze_manifest(src);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn an_unknown_key_is_a_hint_never_an_error() {
        let src = "Module\n  name: game_shop\n  minClientVersion: 1511\n";
        let diags = analyze_manifest(src);
        assert_eq!(codes(&diags), vec![UNKNOWN_MANIFEST_KEY]);
        assert_eq!(diags[0].severity, Severity::Hint);
        assert_eq!(
            &src[diags[0].span.start..diags[0].span.end],
            "minClientVersion"
        );
    }

    #[test]
    fn onload_and_onunload_are_known_events_not_flagged() {
        let src = "Module\n  @onLoad: init()\n  @onUnload: terminate()\n";
        assert!(analyze_manifest(src).is_empty());
    }

    #[test]
    fn scripts_and_dependencies_and_load_later_are_accepted() {
        let src = "Module\n  scripts: [ a, b ]\n  dependencies: [ c ]\n  load-later: [ d ]\n";
        assert!(analyze_manifest(src).is_empty());
    }

    #[test]
    fn a_missing_module_root_is_an_error() {
        let src = "SomethingElse\n  name: x\n";
        let diags = analyze_manifest(src);
        assert_eq!(codes(&diags), vec![MISSING_MODULE_ROOT]);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn an_empty_document_is_also_a_missing_module_root() {
        let diags = analyze_manifest("");
        assert_eq!(codes(&diags), vec![MISSING_MODULE_ROOT]);
    }

    #[test]
    fn a_module_root_found_among_other_top_level_nodes_is_accepted() {
        // The engine's `at("Module")` scans every top-level child for the first non-null match,
        // regardless of position — a manifest with something else declared first (unusual, never
        // observed in the real corpus, but legal OTML) still resolves.
        let src = "SomethingElse\n  x: 1\nModule\n  name: y\n";
        assert!(analyze_manifest(src).is_empty());
    }

    #[test]
    fn a_module_with_an_inline_base_does_not_count_as_the_root() {
        // `Module < Something` parses as a `style_header`, not a bare `container` — and the
        // engine's own `OTMLNode::tag()` for that line is the *literal* "Module < Something" text
        // (`UIManager::importStyleFromOTML` splits on `<` itself, later, only for style imports),
        // so `doc->at("Module")` would not find it either.
        let src = "Module < Something\n  name: x\n";
        let diags = analyze_manifest(src);
        assert_eq!(codes(&diags), vec![MISSING_MODULE_ROOT]);
    }

    #[test]
    fn tab_indentation_is_still_a_hard_error_on_a_manifest() {
        // The structural passes are schema-agnostic and still apply to a manifest.
        let src = "Module\n\tname: x\n";
        let diags = analyze_manifest(src);
        assert!(
            diags
                .iter()
                .any(|d| d.code == crate::diagnostics::TAB_INDENTATION
                    && d.severity == Severity::Error),
            "{diags:?}"
        );
    }

    #[test]
    fn a_syntax_error_is_still_reported_on_a_manifest() {
        let src = "Module\n  name\tbroken: value\n";
        let diags = analyze_manifest(src);
        assert!(
            diags
                .iter()
                .any(|d| d.code == crate::diagnostics::SYNTAX_ERROR),
            "{diags:?}"
        );
    }

    #[test]
    fn no_widget_unknown_property_ever_fires_on_a_manifest() {
        // The regression this whole node exists to fix: `scripts:`/`sandboxed:`/etc. must never be
        // judged against the widget property catalog.
        let src = "\
Module
  name: game_shop
  description: In-game shop
  scripts: [ game_shop ]
  sandboxed: true
  @onLoad: init()
";
        let diags = analyze_manifest(src);
        assert!(
            diags.iter().all(|d| d.code != "unknown-property"),
            "{diags:?}"
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn a_real_engine_manifest_shape_is_accepted_verbatim() {
        // `modules/game_topmenu/topmenu.otmod`-style real manifest shape.
        let src = "\
Module
  name: client_topmenu
  description: Client topmenu
  author: OTClient team
  website: https://github.com/edubart/otclient
  sandboxed: true
  scripts: [ topmenu ]
  @onLoad: init()
  @onUnload: terminate()
";
        assert!(analyze_manifest(src).is_empty());
    }

    #[test]
    fn a_well_formed_font_manifest_has_no_diagnostics() {
        // `data/fonts/otfont/small-9px.otfont`-style real shape.
        let src = "\
Font
  name: small-9px
  texture: small-9px
  height: 9
  glyph-size: 9 9
  space-width: 3
  spacing: 1 0
  y-offset: 0
  first-glyph: 32
  fixed-glyph-width: 9
  default: true
  widget-default: true
";
        let diags = analyze_font_manifest(src);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn an_unknown_font_key_is_a_hint_never_an_error() {
        // `verdana-10px-rounded.otfont`'s real `x-offset:` key — `BitmapFont::load` reads
        // `y-offset` but never `x-offset`.
        let src = "Font\n  name: verdana-10px-rounded\n  x-offset: 1\n";
        let diags = analyze_font_manifest(src);
        assert_eq!(codes(&diags), vec![UNKNOWN_MANIFEST_KEY]);
        assert_eq!(diags[0].severity, Severity::Hint);
        assert_eq!(&src[diags[0].span.start..diags[0].span.end], "x-offset");
    }

    #[test]
    fn a_missing_font_root_is_an_error() {
        let src = "SomethingElse\n  name: x\n";
        let diags = analyze_font_manifest(src);
        assert_eq!(codes(&diags), vec![MISSING_FONT_ROOT]);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn a_module_manifest_key_is_unknown_under_the_font_schema_and_vice_versa() {
        // The two schemas are disjoint: `scripts:` (a module key) is not a font key, and `texture:`
        // (a font key) is not a module key. Each is judged only against its own schema.
        let module_src = "Module\n  name: m\n  texture: t\n";
        let diags = analyze_manifest(module_src);
        assert_eq!(codes(&diags), vec![UNKNOWN_MANIFEST_KEY]);
        assert_eq!(
            &module_src[diags[0].span.start..diags[0].span.end],
            "texture"
        );

        let font_src = "Font\n  name: f\n  scripts: [ x ]\n";
        let diags = analyze_font_manifest(font_src);
        assert_eq!(codes(&diags), vec![UNKNOWN_MANIFEST_KEY]);
        assert_eq!(&font_src[diags[0].span.start..diags[0].span.end], "scripts");
    }
}
