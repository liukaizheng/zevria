//! Pane identity and appearance. Capabilities never depend on a title or theme.

use zevria_foundation::ModelRole;

use crate::{presentation::TranscriptAppearance, status::ExternalContextUsage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticsState {
    Unsupported,
    Hidden,
    Visible,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PaneKind {
    #[default]
    Root,
    LivePlanWorker,
    HistoricalAgent,
    SubtaskInspect,
}

#[derive(Debug, Eq, PartialEq)]
struct PaneMetadata {
    title: String,
    diagnostics: DiagnosticsState,
    appearance: TranscriptAppearance,
    model_role: Option<ModelRole>,
    external_context: Option<ExternalContextUsage>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct PaneState {
    kind: PaneKind,
    metadata: Option<PaneMetadata>,
}

impl PaneState {
    pub(crate) fn subtask_inspect(title: impl Into<String>, model_role: ModelRole) -> Self {
        Self {
            kind: PaneKind::SubtaskInspect,
            metadata: Some(PaneMetadata {
                title: title.into(),
                diagnostics: DiagnosticsState::Unsupported,
                appearance: TranscriptAppearance::Native,
                model_role: Some(model_role),
                external_context: None,
            }),
        }
    }

    pub(crate) fn set_inspect_model_role(&mut self, role: ModelRole) -> bool {
        let Some(metadata) = self.metadata.as_mut() else {
            return false;
        };
        metadata.model_role = Some(role);
        true
    }

    pub(crate) fn acp_inspect(title: impl Into<String>) -> Self {
        Self {
            kind: PaneKind::HistoricalAgent,
            metadata: Some(PaneMetadata {
                title: title.into(),
                diagnostics: DiagnosticsState::Hidden,
                appearance: TranscriptAppearance::Acp,
                model_role: None,
                external_context: None,
            }),
        }
    }

    pub(crate) fn enable_worker(&mut self) {
        if self.kind == PaneKind::HistoricalAgent {
            self.kind = PaneKind::LivePlanWorker;
        }
    }

    pub(crate) fn freeze_worker(&mut self) {
        if self.kind == PaneKind::LivePlanWorker {
            self.kind = PaneKind::HistoricalAgent;
        }
    }

    pub(crate) const fn is_worker(&self) -> bool {
        matches!(self.kind, PaneKind::LivePlanWorker)
    }

    pub(crate) const fn can_compose(&self) -> bool {
        matches!(self.kind, PaneKind::Root | PaneKind::LivePlanWorker)
    }

    pub(crate) const fn is_root(&self) -> bool {
        matches!(self.kind, PaneKind::Root)
    }

    pub(crate) fn title(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.title.as_str())
    }

    pub(crate) fn update_metadata(
        &mut self,
        title: impl Into<String>,
        external_context: Option<ExternalContextUsage>,
    ) -> bool {
        let Some(metadata) = self.metadata.as_mut() else {
            return false;
        };
        metadata.title = title.into();
        metadata.external_context = external_context;
        true
    }

    pub(crate) fn model_role_override(&self) -> Option<ModelRole> {
        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.model_role)
    }

    pub(crate) fn external_context(&self) -> Option<ExternalContextUsage> {
        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.external_context)
    }

    pub(crate) fn appearance(&self) -> TranscriptAppearance {
        self.metadata
            .as_ref()
            .map_or(TranscriptAppearance::Native, |metadata| metadata.appearance)
    }

    pub(crate) fn diagnostics(&self) -> DiagnosticsState {
        self.metadata
            .as_ref()
            .map_or(DiagnosticsState::Unsupported, |metadata| {
                metadata.diagnostics
            })
    }

    pub(crate) fn diagnostics_supported(&self) -> bool {
        self.diagnostics() != DiagnosticsState::Unsupported
    }

    pub(crate) fn diagnostics_visible(&self) -> bool {
        self.diagnostics() == DiagnosticsState::Visible
    }

    pub(crate) fn toggle_diagnostics(&mut self) -> bool {
        let Some(metadata) = self.metadata.as_mut() else {
            return false;
        };
        metadata.diagnostics = match metadata.diagnostics {
            DiagnosticsState::Unsupported => return false,
            DiagnosticsState::Hidden => DiagnosticsState::Visible,
            DiagnosticsState::Visible => DiagnosticsState::Hidden,
        };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_survives_worker_freeze_and_metadata_changes() {
        let mut pane = PaneState::acp_inspect("not an ACP-looking title");
        pane.enable_worker();
        pane.update_metadata("native-looking", None);
        pane.toggle_diagnostics();
        assert_eq!(pane.appearance(), TranscriptAppearance::Acp);
        assert!(pane.can_compose());
        pane.freeze_worker();
        assert_eq!(pane.appearance(), TranscriptAppearance::Acp);
        assert!(!pane.can_compose());
        assert_eq!(
            PaneState::default().appearance(),
            TranscriptAppearance::Native
        );
        let mut child = PaneState::subtask_inspect("ACP", ModelRole::Explore);
        child.enable_worker();
        assert!(
            !child.can_compose(),
            "a subtask cannot become a Plan worker"
        );
        assert_eq!(child.appearance(), TranscriptAppearance::Native);
    }

    #[test]
    fn diagnostics_cannot_be_enabled_on_unsupported_panes() {
        let mut root = PaneState::default();
        assert!(!root.toggle_diagnostics());
        assert_eq!(root.diagnostics(), DiagnosticsState::Unsupported);
        let mut subtask = PaneState::subtask_inspect("child", ModelRole::Explore);
        assert!(!subtask.toggle_diagnostics());
        let mut acp = PaneState::acp_inspect("agent");
        assert!(acp.toggle_diagnostics());
        assert_eq!(acp.diagnostics(), DiagnosticsState::Visible);
    }
}
