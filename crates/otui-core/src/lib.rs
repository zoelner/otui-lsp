//! The pure OTUI/OTML language engine.
//!
//! `otui-core` holds all language semantics — parsing, diagnostics, symbols, completion and
//! formatting — with **no I/O and no dependency on `lsp-types`**. Everything it returns is
//! expressed in byte offsets via the [`lang-api`] contract, so the same engine can back the LSP
//! server or be embedded directly in an editor.
//!
//! Behavior is a faithful mirror of the real OTClient engine, per the spec vendored at
//! `docs/otui-language-service-spec.md`. Milestones fill in the submodules
//! (`syntax`, `schema`, `index`, `diagnostics`, `completion`, `symbols`, `format`); this M1
//! scaffold only wires the [`LanguageService`] entry point.

use lang_api::{Diagnostic, LanguageService};

/// The OTUI language backend. Constructed once per workspace/session.
#[derive(Debug, Default)]
pub struct OtuiService {
    _private: (),
}

impl OtuiService {
    pub fn new() -> Self {
        Self::default()
    }
}

impl LanguageService for OtuiService {
    fn language_id(&self) -> &'static str {
        "otui"
    }

    fn diagnostics(&self, _source: &str) -> Vec<Diagnostic> {
        // Diagnostics are implemented from M3 onward (spec §4). For now, no findings.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_reports_its_language_id() {
        let svc = OtuiService::new();
        assert_eq!(svc.language_id(), "otui");
    }

    #[test]
    fn scaffold_produces_no_diagnostics_yet() {
        let svc = OtuiService::new();
        assert!(svc
            .diagnostics("MainWindow < UIWindow\n  id: main\n")
            .is_empty());
    }
}
