//! Mode/Safety/Permissions policy and the single-use capability grant a
//! resolver issues for one approved call.

use super::*;

/// Legacy decision mirror kept for existing callers; new code should use
/// [`crate::safety::Decision`] and the richer classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny(String),
    Ask(String),
}

impl From<safety::Decision> for PolicyDecision {
    fn from(decision: safety::Decision) -> Self {
        match decision {
            safety::Decision::Allow => Self::Allow,
            safety::Decision::Ask(reason) => Self::Ask(reason),
            safety::Decision::Deny(reason) => Self::Deny(reason),
        }
    }
}

/// Capabilities a resolver approved for exactly one call. Grants never
/// override a `Deny`; they only convert the matching `Ask` into an allow with
/// the narrowest capability set.
#[derive(Debug, Clone, Default)]
pub struct CapabilityGrant {
    pub capabilities: CapabilitySet,
    pub external_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    mode: Arc<RwLock<Mode>>,
    safety: Arc<RwLock<Safety>>,
    permissions: Arc<RwLock<PermissionMode>>,
    workspace: PathBuf,
    pub(super) config: PermissionConfig,
}

impl PolicyEngine {
    #[must_use]
    pub fn new(mode: Mode, workspace: PathBuf, config: PermissionConfig) -> Self {
        Self {
            mode: Arc::new(RwLock::new(mode)),
            safety: Arc::new(RwLock::new(Safety::Standard)),
            permissions: Arc::new(RwLock::new(config.mode)),
            workspace,
            config,
        }
    }
    /// Constructor used by the CLI, which owns the full configuration and can
    /// supply the configured default safety profile.
    #[must_use]
    pub fn with_defaults(
        mode: Mode,
        workspace: PathBuf,
        config: PermissionConfig,
        safety: Safety,
    ) -> Self {
        let engine = Self::new(mode, workspace, config);
        engine.set_safety(safety);
        engine
    }
    pub fn set_mode(&self, mode: Mode) {
        if let Ok(mut current) = self.mode.write() {
            *current = mode;
        }
    }
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode.read().map_or(Mode::Ask, |mode| *mode)
    }
    pub fn set_safety(&self, safety: Safety) {
        if let Ok(mut current) = self.safety.write() {
            *current = safety;
        }
    }
    #[must_use]
    pub fn safety(&self) -> Safety {
        self.safety
            .read()
            .map_or(Safety::Standard, |safety| *safety)
    }
    pub fn set_permissions(&self, mode: PermissionMode) {
        if let Ok(mut current) = self.permissions.write() {
            *current = mode;
        }
    }
    #[must_use]
    pub fn permissions(&self) -> PermissionMode {
        self.permissions
            .read()
            .map_or(PermissionMode::Human, |mode| *mode)
    }
    /// Classifies one call through the safety layer.
    #[must_use]
    pub fn classify(&self, tool: &str, args: &Value) -> safety::Classification {
        safety::classify(
            tool,
            args,
            safety::Context {
                mode: self.mode(),
                safety: self.safety(),
                workspace: &self.workspace,
                outside: self.config.outside_workspace,
                workspace_write: self.config.workspace_write,
            },
        )
    }
    /// Decision only; retained for callers that do not need capabilities.
    #[must_use]
    pub fn decide(&self, tool: &str, args: &Value) -> PolicyDecision {
        match self.classify(tool, args).decision {
            safety::Decision::Allow => PolicyDecision::Allow,
            safety::Decision::Ask(reason) => PolicyDecision::Ask(reason),
            safety::Decision::Deny(reason) => PolicyDecision::Deny(reason),
        }
    }
}
