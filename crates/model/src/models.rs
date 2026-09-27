//! Provider-neutral runtime model management. Management never becomes a turn.

use crate::{ContextTokenEstimate, ModelContextPolicy, ModelProfileRef, ModelRole, SessionMode};
use zevria_foundation::ReasoningLevel;

/// An exact profile identity and an explicitly selected supported reasoning level.
/// Reasoning is a request property, never part of the profile/cache identity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub profile: ModelProfileRef,
    pub reasoning_level: ReasoningLevel,
}

impl ModelSelection {
    pub fn new(profile: ModelProfileRef, reasoning_level: ReasoningLevel) -> Self {
        Self {
            profile,
            reasoning_level,
        }
    }
}

impl std::fmt::Display for ModelSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} · {}", self.profile, self.reasoning_level)
    }
}

/// Catalog capabilities, independent of any role's selected level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCandidate {
    pub context: ModelContextPolicy,
    pub reasoning_levels: Vec<ReasoningLevel>,
}

pub const SESSION_MODELS_VERSION: u32 = 1;

/// Complete durable selections for the two session-owned roles.
/// No reasoning defaults may be inferred on resume.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "SessionModelsWire")]
pub struct SessionModels {
    version: u32,
    build: ModelSelection,
    plan: ModelSelection,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionModelsWire {
    version: u32,
    build: ModelSelection,
    plan: ModelSelection,
}

impl TryFrom<SessionModelsWire> for SessionModels {
    type Error = anyhow::Error;

    fn try_from(value: SessionModelsWire) -> anyhow::Result<Self> {
        anyhow::ensure!(
            value.version == SESSION_MODELS_VERSION,
            "unsupported session models version {}; expected {SESSION_MODELS_VERSION} with complete Build/Plan selections; start a new session or explicitly repair the saved metadata",
            value.version
        );
        Self::new(value.build, value.plan)
    }
}

impl SessionModels {
    pub fn new(build: ModelSelection, plan: ModelSelection) -> anyhow::Result<Self> {
        for (role, selection) in [("Build", &build), ("Plan", &plan)] {
            anyhow::ensure!(
                !selection.profile.provider.trim().is_empty()
                    && !selection.profile.model.trim().is_empty(),
                "session {role} selection requires nonblank provider and model identities"
            );
        }
        Ok(Self {
            version: SESSION_MODELS_VERSION,
            build,
            plan,
        })
    }

    pub fn for_mode(&self, mode: SessionMode) -> &ModelSelection {
        match mode {
            SessionMode::Build => &self.build,
            SessionMode::Plan => &self.plan,
        }
    }

    pub fn reasoning_for_mode(&self, mode: SessionMode) -> ReasoningLevel {
        self.for_mode(mode).reasoning_level
    }

    /// Change the level within the complete selection; never an optional override.
    pub fn with_reasoning(&self, mode: SessionMode, level: ReasoningLevel) -> Self {
        let mut next = self.clone();
        match mode {
            SessionMode::Build => next.build.reasoning_level = level,
            SessionMode::Plan => next.plan.reasoning_level = level,
        }
        next
    }

    pub fn with_selection(
        &self,
        mode: SessionMode,
        target: ModelSelection,
    ) -> anyhow::Result<Self> {
        match mode {
            SessionMode::Build => Self::new(target, self.plan.clone()),
            SessionMode::Plan => Self::new(self.build.clone(), target),
        }
    }
}

/// Opaque checkpoints need an explicitly confirmed source-profile summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayPreflight {
    Compatible(ContextTokenEstimate),
    ConversionRequired { sources: Vec<ModelProfileRef> },
}

/// Both scopes are durable for this session; only SessionAndDefault writes config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSelectionScope {
    SessionOnly,
    SessionAndDefault,
}

impl ModelSelectionScope {
    pub const fn command(self) -> &'static str {
        match self {
            Self::SessionOnly => "/model-session",
            Self::SessionAndDefault => "/model",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelManagementRequest {
    List {
        mode: SessionMode,
        scope: ModelSelectionScope,
    },
    Select {
        mode: SessionMode,
        scope: ModelSelectionScope,
        target: ModelSelection,
        revision: String,
    },
    Confirm {
        preview: ModelSelectionPreview,
    },
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelectionPreview {
    pub request_id: String,
    /// Fresh for each runtime, including reopening the same transcript.
    pub session_generation: String,
    pub generation: u64,
    pub mode: SessionMode,
    pub scope: ModelSelectionScope,
    pub target: ModelSelection,
    pub source: ModelSelection,
    pub revision: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelManagementResult {
    Catalog {
        mode: SessionMode,
        scope: ModelSelectionScope,
        current: ModelSelection,
        profiles: Vec<ModelCandidate>,
        revision: String,
    },
    ConfirmationRequired(ModelSelectionPreview),
    Changed {
        role: ModelRole,
        scope: ModelSelectionScope,
        context: ModelContextPolicy,
        reasoning_level: ReasoningLevel,
        snapshot: Option<crate::ContextTokenSnapshot>,
        revision: String,
        unchanged: bool,
    },
    Cancelled,
    Rejected {
        code: String,
        message: String,
        checkpoint_installed: bool,
        /// Present after config committed but the session header failed.
        current_revision: Option<String>,
    },
}

impl ModelManagementResult {
    pub fn rejected(code: &str, message: impl Into<String>) -> Self {
        Self::Rejected {
            code: code.into(),
            message: message.into(),
            checkpoint_installed: false,
            current_revision: None,
        }
    }
}

/// Short local settings operations; no config lock crosses provider inference.
pub trait ModelSettingsService: Send + Sync + 'static {
    fn validate(&self, expected_revision: &str) -> anyhow::Result<()>;
    fn save(
        &self,
        expected_revision: &str,
        role: ModelRole,
        target: &ModelSelection,
    ) -> anyhow::Result<String>;
}

pub const fn mode_role(mode: SessionMode) -> ModelRole {
    match mode {
        SessionMode::Build => ModelRole::Build,
        SessionMode::Plan => ModelRole::Plan,
    }
}
