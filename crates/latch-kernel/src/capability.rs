//! Runtime capability vocabulary: the kernel-owned names, scopes, owners, and
//! lifetimes that describe what a Latch session currently offers.
//!
//! This module is deliberately **not** a service container and **not** a hook
//! bus. It is a small, immutable vocabulary plus a declaration registry:
//!
//! - a [`CapabilityDescriptor`] states one capability's kind, owner, lifetime,
//!   scope, and permission ceiling;
//! - a [`CapabilityRegistry`] holds the declarations the kernel itself made and
//!   resolves a [`CapabilityRequest`] only against those declarations;
//! - a [`CapabilityHandle`] is an immutable value with no API to widen scope,
//!   extend lifetime, or upgrade permissions after issuance.
//!
//! Authority stays with the kernel. Extensions, backends, clients, and MCP
//! servers can *offer* capabilities, but only kernel code declares them here,
//! and no declaration can grant kernel truth: per-call enforcement remains
//! [`crate::safety`] plus the mandatory sandbox. The normative model, including
//! the full list of kernel invariants no capability can bypass, is
//! `docs/RUNTIME_CAPABILITY_MODEL.md`.
//!
//! Scope descriptors reference the same OS-level permission vocabulary the
//! sandbox already enforces ([`crate::sandbox::Capability`]), so a declared
//! capability and its actual enforcement do not drift apart.

use crate::sandbox::CapabilitySet;
use anyhow::{Result, bail};
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

/// Maximum accepted symbolic capability id length.
const MAX_ID_BYTES: usize = 96;

/// One runtime capability category. These are the classes future features map
/// onto; only the classes with a live mechanism are declared by a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CapabilityKind {
    /// Workspace filesystem access (today the local workspace; later LayerFS
    /// or another `WorkspaceBackend`).
    Workspace,
    /// Command/process execution behind the mandatory sandbox.
    Executor,
    /// Context materialization (the [`crate::context::ContextEngine`] port).
    Context,
    /// Model-facing tools: builtin, validation, agent controls, extensions.
    Tools,
    /// GUI/desktop control on a local or remote machine (Computer Use).
    Computer,
    /// Browser automation.
    Browser,
    /// Host-managed service exposure (dev server, TensorBoard, Jupyter, VNC).
    Service,
    /// Durable artifacts (session artifact store, spilled process output).
    Artifacts,
    /// Child agents and coordination (supervisor, group overlay).
    Agents,
}

impl CapabilityKind {
    /// Stable lowercase diagnostic name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Executor => "executor",
            Self::Context => "context",
            Self::Tools => "tools",
            Self::Computer => "computer",
            Self::Browser => "browser",
            Self::Service => "service",
            Self::Artifacts => "artifacts",
            Self::Agents => "agents",
        }
    }
}

impl fmt::Display for CapabilityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Stable symbolic capability identifier, for example `workspace.primary`.
///
/// Ids are kernel-declared literals; the validated constructor exists for
/// declarations and tests, not for model or extension input.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityId(String);

impl CapabilityId {
    /// Validates and constructs a capability id.
    pub fn new(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        Self::validate(&raw)?;
        Ok(Self(raw))
    }

    /// Kernel-declared id. The literal is a compile-time constant; invalid
    /// literals fail debug assertions and are covered by unit tests.
    pub(crate) fn kernel(raw: &'static str) -> Self {
        debug_assert!(
            Self::validate(raw).is_ok(),
            "kernel capability id `{raw}` is invalid"
        );
        Self(raw.to_owned())
    }

    fn validate(raw: &str) -> Result<()> {
        if raw.is_empty() {
            bail!("capability id must not be empty");
        }
        if raw.len() > MAX_ID_BYTES {
            bail!("capability id exceeds {MAX_ID_BYTES} bytes");
        }
        if !raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        {
            bail!("capability id may only contain ASCII alphanumerics, '.', '-', and '_'");
        }
        Ok(())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Who owns one declared capability instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityOwner {
    /// The kernel itself: kernel-owned state, ports, and bookkeeping.
    Kernel,
    /// A durable session (root or child), identified by its session id.
    Session(Uuid),
    /// A configured, sandboxed extension host, keyed by its stable name.
    Extension(String),
    /// An authenticated client adapter (TUI, remote app-protocol client).
    Client(String),
    /// A host-managed auxiliary service.
    Service(String),
}

/// How long a declaration or issued handle is valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityLifetime {
    /// The whole durable session, across resume.
    Session(Uuid),
    /// Exactly one run.
    Run(Uuid),
    /// While one process or connection is alive.
    Connection,
    /// One binding/tool call.
    Call(String),
}

/// The bounded surface a capability may act on. Scope containment is strict and
/// one-directional: a request is only ever contained by a wider declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityScope {
    /// The owning session only; no filesystem, network, or process surface.
    Session,
    /// A workspace root; all paths must stay inside it.
    Workspace { root: PathBuf },
    /// Explicit network endpoints for host-managed service exposure.
    Network { hosts: Vec<String>, ports: Vec<u16> },
    /// Exactly one remote endpoint (for example a remote Computer backend).
    Remote { endpoint: String },
}

impl CapabilityScope {
    /// True when `candidate` stays inside this scope.
    #[must_use]
    pub fn contains(&self, candidate: &CapabilityScope) -> bool {
        match (self, candidate) {
            (Self::Session, Self::Session) => true,
            (Self::Workspace { root }, Self::Workspace { root: candidate }) => {
                candidate.starts_with(root)
            }
            (
                Self::Network { hosts, ports },
                Self::Network {
                    hosts: candidate_hosts,
                    ports: candidate_ports,
                },
            ) => {
                candidate_hosts.iter().all(|host| hosts.contains(host))
                    && candidate_ports.iter().all(|port| ports.contains(port))
            }
            (
                Self::Remote { endpoint },
                Self::Remote {
                    endpoint: candidate,
                },
            ) => endpoint == candidate,
            _ => false,
        }
    }
}

/// One kernel declaration: what a capability is, who owns it, how long it
/// lives, exactly what it may reach, and which OS-level permissions it may
/// exercise.
#[derive(Debug, Clone)]
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub kind: CapabilityKind,
    pub owner: CapabilityOwner,
    pub lifetime: CapabilityLifetime,
    pub scope: CapabilityScope,
    /// Permission ceiling, expressed in the same vocabulary the sandbox
    /// enforces. Declaring a permission never grants it; a call still passes
    /// through safety classification and the sandbox profile.
    pub permissions: CapabilitySet,
}

/// A request for a surface, resolved against kernel declarations. A request can
/// never name kernel authority directly; it names a kind, scope, and permission
/// set, and the registry decides whether some declaration covers it.
#[derive(Debug, Clone)]
pub struct CapabilityRequest {
    pub kind: CapabilityKind,
    pub scope: CapabilityScope,
    pub permissions: CapabilitySet,
}

/// An immutable issued descriptor. Handles are values: there is no API to
/// widen scope, extend lifetime, or upgrade permissions after issuance.
#[derive(Debug, Clone)]
pub struct CapabilityHandle(CapabilityDescriptor);

impl CapabilityHandle {
    #[must_use]
    pub fn new(descriptor: CapabilityDescriptor) -> Self {
        Self(descriptor)
    }

    #[must_use]
    pub fn descriptor(&self) -> &CapabilityDescriptor {
        &self.0
    }

    #[must_use]
    pub fn id(&self) -> &CapabilityId {
        &self.0.id
    }

    #[must_use]
    pub fn kind(&self) -> CapabilityKind {
        self.0.kind
    }

    /// True when this handle's declaration covers the requested kind, scope,
    /// and permissions.
    #[must_use]
    pub fn allows(&self, request: &CapabilityRequest) -> bool {
        self.0.kind == request.kind
            && self.0.scope.contains(&request.scope)
            && request.permissions.is_subset(&self.0.permissions)
    }
}

/// The kernel-owned declaration registry for one session. Duplicate ids are
/// rejected; resolution only ever answers from declared capabilities.
#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    descriptors: Vec<CapabilityDescriptor>,
}

impl CapabilityRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares one kernel-owned capability.
    pub fn declare(&mut self, descriptor: CapabilityDescriptor) -> Result<()> {
        if self
            .descriptors
            .iter()
            .any(|existing| existing.id == descriptor.id)
        {
            bail!("capability {} is already declared", descriptor.id);
        }
        self.descriptors.push(descriptor);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&CapabilityDescriptor> {
        self.descriptors
            .iter()
            .find(|entry| entry.id.as_str() == id)
    }

    #[must_use]
    pub fn descriptors(&self) -> &[CapabilityDescriptor] {
        &self.descriptors
    }

    #[must_use]
    pub fn handles(&self) -> Vec<CapabilityHandle> {
        self.descriptors
            .iter()
            .cloned()
            .map(CapabilityHandle::new)
            .collect()
    }

    /// Resolves a request to the first declaration that fully covers it, or
    /// `None` when the kernel declared no such surface.
    #[must_use]
    pub fn resolve(&self, request: &CapabilityRequest) -> Option<&CapabilityDescriptor> {
        self.descriptors.iter().find(|descriptor| {
            descriptor.kind == request.kind
                && descriptor.scope.contains(&request.scope)
                && request.permissions.is_subset(&descriptor.permissions)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Capability;

    fn descriptor(id: &'static str, scope: CapabilityScope) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: CapabilityId::kernel(id),
            kind: CapabilityKind::Workspace,
            owner: CapabilityOwner::Kernel,
            lifetime: CapabilityLifetime::Session(Uuid::new_v4()),
            scope,
            permissions: CapabilitySet::new(),
        }
    }

    #[test]
    fn capability_ids_are_validated() {
        assert!(CapabilityId::new("workspace.primary").is_ok());
        assert!(CapabilityId::new("").is_err());
        assert!(CapabilityId::new("workspace primary").is_err());
        assert!(CapabilityId::new("a".repeat(MAX_ID_BYTES + 1)).is_err());
        assert_eq!(
            CapabilityId::kernel("workspace.primary").as_str(),
            "workspace.primary"
        );
    }

    #[test]
    fn scope_containment_is_strict() {
        let root = PathBuf::from("/tmp/ws");
        let workspace = CapabilityScope::Workspace { root: root.clone() };
        assert!(workspace.contains(&CapabilityScope::Workspace {
            root: root.join("src"),
        }));
        assert!(!workspace.contains(&CapabilityScope::Workspace {
            root: PathBuf::from("/tmp/elsewhere"),
        }));
        let network = CapabilityScope::Network {
            hosts: vec!["127.0.0.1".into()],
            ports: vec![8080],
        };
        assert!(network.contains(&CapabilityScope::Network {
            hosts: vec!["127.0.0.1".into()],
            ports: vec![8080],
        }));
        assert!(!network.contains(&CapabilityScope::Network {
            hosts: vec!["127.0.0.1".into()],
            ports: vec![9090],
        }));
        assert!(!workspace.contains(&CapabilityScope::Session));
    }

    #[test]
    fn handles_never_widen_scope_or_permissions() {
        let root = PathBuf::from("/tmp/ws");
        let mut permissions = CapabilitySet::new();
        permissions.insert(Capability::WorkspaceRead);
        let handle = CapabilityHandle::new(CapabilityDescriptor {
            id: CapabilityId::kernel("workspace.primary"),
            kind: CapabilityKind::Workspace,
            owner: CapabilityOwner::Session(Uuid::new_v4()),
            lifetime: CapabilityLifetime::Session(Uuid::new_v4()),
            scope: CapabilityScope::Workspace { root: root.clone() },
            permissions: permissions.clone(),
        });
        let inside = CapabilityRequest {
            kind: CapabilityKind::Workspace,
            scope: CapabilityScope::Workspace {
                root: root.join("src"),
            },
            permissions: permissions.clone(),
        };
        assert!(handle.allows(&inside));
        let outside = CapabilityRequest {
            scope: CapabilityScope::Workspace {
                root: PathBuf::from("/etc"),
            },
            ..inside.clone()
        };
        assert!(!handle.allows(&outside));
        let mut escalated = CapabilitySet::new();
        escalated.insert(Capability::NetworkAccess);
        let network = CapabilityRequest {
            kind: CapabilityKind::Workspace,
            scope: CapabilityScope::Workspace { root: root.clone() },
            permissions: escalated,
        };
        assert!(!handle.allows(&network));
        let other_kind = CapabilityRequest {
            kind: CapabilityKind::Computer,
            ..inside
        };
        assert!(!handle.allows(&other_kind));
    }

    #[test]
    fn registry_rejects_duplicates_and_resolves_declared_only() {
        let root = PathBuf::from("/tmp/ws");
        let mut registry = CapabilityRegistry::new();
        registry
            .declare(descriptor(
                "workspace.primary",
                CapabilityScope::Workspace { root: root.clone() },
            ))
            .unwrap();
        assert!(
            registry
                .declare(descriptor("workspace.primary", CapabilityScope::Session))
                .is_err()
        );
        let request = CapabilityRequest {
            kind: CapabilityKind::Workspace,
            scope: CapabilityScope::Workspace {
                root: root.join("src"),
            },
            permissions: CapabilitySet::new(),
        };
        assert!(registry.resolve(&request).is_some());
        let undeclared = CapabilityRequest {
            kind: CapabilityKind::Computer,
            scope: CapabilityScope::Remote {
                endpoint: "host:1".into(),
            },
            permissions: CapabilitySet::new(),
        };
        assert!(registry.resolve(&undeclared).is_none());
        assert_eq!(registry.handles().len(), 1);
        assert!(registry.get("workspace.primary").is_some());
        assert!(registry.get("browser.primary").is_none());
    }
}
