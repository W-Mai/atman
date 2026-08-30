use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::event::FlowRunId;
use crate::tool::Tier;
use crate::trust::{
    ExecutionPolicy, PolicyAction, PolicyEscalation, PolicyResolution, RiskKind, TrustConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationKind {
    Root,
    InlineSubflow,
    SpawnSync,
    SpawnAsync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowExecutionState {
    Running,
    BlockedOnDescendants {
        child_run_counts: HashMap<FlowRunId, usize>,
    },
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildWorkspaceAuthority {
    Inherit,
    Narrow(PathBuf),
    TrustedDelegation(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAuthority {
    pub execution_policy: ExecutionPolicy,
    pub allowed_tiers: [bool; 5],
    pub allowed_risks: BTreeSet<RiskKind>,
    pub tier_ceiling: [PolicyAction; 5],
    pub risk_ceiling: [PolicyAction; 6],
    pub shell: bool,
    pub permission_management: bool,
    pub workspace_root: Option<PathBuf>,
}

impl EffectiveAuthority {
    pub fn root(_trust: &TrustConfig, shell: bool, workspace_root: Option<PathBuf>) -> Self {
        let risks = all_risks();
        Self {
            // Session trust is evaluated per invocation. Root authority records
            // structural ceilings only, so a user policy change is not frozen at
            // flow start while delegated child restrictions remain monotonic.
            execution_policy: ExecutionPolicy::Unrestricted,
            allowed_tiers: [true; 5],
            allowed_risks: risks.into_iter().collect(),
            tier_ceiling: [PolicyAction::Auto; 5],
            risk_ceiling: [PolicyAction::Auto; 6],
            shell,
            // The session root is the trust boundary for permission decisions.
            // Child authorities can only retain this bit through intersection.
            permission_management: true,
            workspace_root,
        }
    }

    pub fn for_child(
        &self,
        requested: &Self,
        contract_allows_shell: bool,
        workspace_root: Option<PathBuf>,
    ) -> Result<Self, &'static str> {
        let execution_policy = match (self.execution_policy, requested.execution_policy) {
            (ExecutionPolicy::Unrestricted, ExecutionPolicy::Unrestricted) => {
                ExecutionPolicy::Unrestricted
            }
            _ => ExecutionPolicy::Controlled,
        };
        let allowed_tiers = std::array::from_fn(|index| {
            self.allowed_tiers[index] && requested.allowed_tiers[index]
        });
        let allowed_risks = self
            .allowed_risks
            .intersection(&requested.allowed_risks)
            .copied()
            .collect();
        let tier_ceiling = std::array::from_fn(|index| {
            self.tier_ceiling[index].most_restrictive(requested.tier_ceiling[index])
        });
        let risk_ceiling = std::array::from_fn(|index| {
            self.risk_ceiling[index].most_restrictive(requested.risk_ceiling[index])
        });
        Ok(Self {
            execution_policy,
            allowed_tiers,
            allowed_risks,
            tier_ceiling,
            risk_ceiling,
            shell: self.shell && requested.shell && contract_allows_shell,
            permission_management: self.permission_management && requested.permission_management,
            workspace_root: narrowed_workspace(self.workspace_root.as_deref(), workspace_root)?,
        })
    }

    pub fn constrain_policy(
        &self,
        trust: &TrustConfig,
        tier: Tier,
        risks: impl IntoIterator<Item = RiskKind>,
    ) -> (ExecutionPolicy, PolicyAction) {
        let (execution, resolution) = self.constrain_policy_resolution(trust, tier, risks);
        (execution, resolution.action)
    }

    pub fn constrain_policy_resolution(
        &self,
        trust: &TrustConfig,
        tier: Tier,
        risks: impl IntoIterator<Item = RiskKind>,
    ) -> (ExecutionPolicy, PolicyResolution) {
        let current_execution = match (self.execution_policy, trust.execution_policy()) {
            (ExecutionPolicy::Unrestricted, ExecutionPolicy::Unrestricted) => {
                ExecutionPolicy::Unrestricted
            }
            _ => ExecutionPolicy::Controlled,
        };
        if current_execution == ExecutionPolicy::Unrestricted {
            return (
                current_execution,
                PolicyResolution {
                    action: PolicyAction::Auto,
                    escalation: PolicyEscalation::None,
                },
            );
        }
        let tier_index = match tier {
            Tier::Zero => 0,
            Tier::One => 1,
            Tier::Two => 2,
            Tier::Three => 3,
            Tier::Four => 4,
        };
        let risks: Vec<_> = risks.into_iter().collect();
        let ceiling = risks
            .iter()
            .fold(self.tier_ceiling[tier_index], |action, risk| {
                action.most_restrictive(self.risk_ceiling[risk_index(*risk)])
            });
        let mut resolution = trust.resolve_policy_resolution(tier, risks.iter().copied());
        if resolution.escalation == PolicyEscalation::Denied
            && risks.contains(&RiskKind::ProcessSpawn)
            && risks.iter().all(|risk| {
                *risk == RiskKind::ProcessSpawn || trust.resolve_risk(*risk) == PolicyAction::Auto
            })
        {
            // Eager Deny rejects direct process elevation, not the safe
            // sandboxed baseline. Explicit Deny actions never carry the
            // `Denied` escalation marker and therefore remain final.
            resolution.action = PolicyAction::Auto;
        }
        resolution.action = resolution.action.most_restrictive(ceiling);
        (current_execution, resolution)
    }

    pub fn inherited_child(
        &self,
        contract_allows_shell: bool,
        workspace: ChildWorkspaceAuthority,
    ) -> Result<Self, &'static str> {
        match workspace {
            ChildWorkspaceAuthority::Inherit => self.for_child(self, contract_allows_shell, None),
            ChildWorkspaceAuthority::Narrow(workspace_root) => {
                self.for_child(self, contract_allows_shell, Some(workspace_root))
            }
            ChildWorkspaceAuthority::TrustedDelegation(workspace_root) => {
                let mut child = self.for_child(self, contract_allows_shell, None)?;
                child.workspace_root = Some(crate::fs_access::canonicalize_stable(&workspace_root));
                Ok(child)
            }
        }
    }
}

#[derive(Debug)]
pub struct FlowIdentity {
    pub session_id: String,
    pub run_id: FlowRunId,
    pub parent_run_id: Option<FlowRunId>,
    pub root_run_id: FlowRunId,
    pub invocation: InvocationKind,
    pub effective_authority: EffectiveAuthority,
    pub(crate) execution_state: Mutex<FlowExecutionState>,
}

impl FlowIdentity {
    pub fn execution_state(&self) -> FlowExecutionState {
        self.execution_state.lock().unwrap().clone()
    }
}

pub fn contract_allows_shell(contract: Option<&atman_dsl::ast::Contract>) -> bool {
    contract.is_some_and(|contract| {
        contract.blocks.iter().any(|block| {
            block.name.name == "capabilities"
                && block.kwargs.iter().any(|(name, value)| {
                    name.name == "shell"
                        && matches!(
                            value,
                            atman_dsl::ast::Expr::Literal(atman_dsl::ast::Literal::Bool(true))
                        )
                })
        })
    })
}

fn narrowed_workspace(
    parent: Option<&Path>,
    requested: Option<PathBuf>,
) -> Result<Option<PathBuf>, &'static str> {
    match (parent, requested) {
        (Some(parent), Some(requested)) => {
            if requested
                .components()
                .any(|component| component == std::path::Component::ParentDir)
            {
                return Err("child workspace must not contain parent traversal");
            }
            let parent = crate::fs_access::canonicalize_stable(parent);
            let requested = crate::fs_access::canonicalize_stable(&requested);
            if requested.starts_with(&parent) {
                Ok(Some(requested))
            } else {
                Err("child workspace must be within the parent workspace")
            }
        }
        (Some(parent), None) => Ok(Some(crate::fs_access::canonicalize_stable(parent))),
        (None, None) => Ok(None),
        (None, Some(_)) => Err("child cannot acquire workspace authority absent from its parent"),
    }
}

fn risk_index(risk: RiskKind) -> usize {
    match risk {
        RiskKind::WorkspaceExternal => 0,
        RiskKind::Network => 1,
        RiskKind::Irreversible => 2,
        RiskKind::FilesystemWrite => 3,
        RiskKind::ProcessSpawn => 4,
        RiskKind::RepositoryMutation => 5,
    }
}

fn all_risks() -> [RiskKind; 6] {
    [
        RiskKind::WorkspaceExternal,
        RiskKind::Network,
        RiskKind::Irreversible,
        RiskKind::FilesystemWrite,
        RiskKind::ProcessSpawn,
        RiskKind::RepositoryMutation,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_authority_intersects_every_capability_field() {
        let parent = EffectiveAuthority {
            execution_policy: ExecutionPolicy::Controlled,
            allowed_tiers: [true, false, true, false, true],
            allowed_risks: BTreeSet::from([RiskKind::Network, RiskKind::FilesystemWrite]),
            tier_ceiling: [PolicyAction::Auto; 5],
            risk_ceiling: [PolicyAction::Auto; 6],
            shell: true,
            permission_management: false,
            workspace_root: None,
        };
        let requested = EffectiveAuthority {
            execution_policy: ExecutionPolicy::Unrestricted,
            allowed_tiers: [false, true, true, true, false],
            allowed_risks: BTreeSet::from([RiskKind::Network, RiskKind::ProcessSpawn]),
            tier_ceiling: [PolicyAction::Auto; 5],
            risk_ceiling: [PolicyAction::Auto; 6],
            shell: false,
            permission_management: true,
            workspace_root: None,
        };

        let child = parent.for_child(&requested, true, None).unwrap();

        assert_eq!(child.execution_policy, ExecutionPolicy::Controlled);
        assert_eq!(child.allowed_tiers, [false, false, true, false, false]);
        assert_eq!(child.allowed_risks, BTreeSet::from([RiskKind::Network]));
        assert!(!child.shell);
        assert!(!child.permission_management);
    }

    #[test]
    fn root_authority_applies_each_live_session_policy_snapshot() {
        let started_controlled = TrustConfig::default();
        let root = EffectiveAuthority::root(&started_controlled, true, None);
        let reckless = TrustConfig {
            mode: crate::trust::TrustMode::Reckless,
            ..TrustConfig::default()
        };

        assert_eq!(
            root.constrain_policy(&reckless, Tier::Four, [RiskKind::ProcessSpawn]),
            (ExecutionPolicy::Unrestricted, PolicyAction::Auto)
        );
        assert_eq!(
            root.constrain_policy(&started_controlled, Tier::Four, [RiskKind::ProcessSpawn]),
            (ExecutionPolicy::Controlled, PolicyAction::Ask)
        );

        let restricted = EffectiveAuthority {
            execution_policy: ExecutionPolicy::Controlled,
            tier_ceiling: [PolicyAction::Deny; 5],
            ..root.clone()
        };
        let child = root.for_child(&restricted, true, None).unwrap();
        assert_eq!(
            child.constrain_policy(&reckless, Tier::Four, [RiskKind::ProcessSpawn]),
            (ExecutionPolicy::Controlled, PolicyAction::Deny)
        );
        assert_eq!(child.shell, root.shell);
        assert_eq!(child.workspace_root, root.workspace_root);
    }

    #[test]
    fn authority_ceiling_prevents_eager_allow_from_becoming_automatic() {
        let trust = TrustConfig {
            mode: crate::trust::TrustMode::Eager,
            escalation: crate::trust::EscalationPolicy::Allow,
            ..TrustConfig::default()
        };
        let authority = EffectiveAuthority {
            execution_policy: ExecutionPolicy::Controlled,
            tier_ceiling: [PolicyAction::Ask; 5],
            ..EffectiveAuthority::root(&trust, true, None)
        };

        let (execution, resolution) =
            authority.constrain_policy_resolution(&trust, Tier::Two, [RiskKind::ProcessSpawn]);

        assert_eq!(execution, ExecutionPolicy::Controlled);
        assert_eq!(resolution.action, PolicyAction::Ask);
        assert_eq!(resolution.escalation, PolicyEscalation::Allowed);
    }

    #[test]
    fn eager_deny_keeps_sandboxable_processes_automatic() {
        let trust = TrustConfig {
            mode: crate::trust::TrustMode::Eager,
            escalation: crate::trust::EscalationPolicy::Deny,
            ..TrustConfig::default()
        };
        let root = EffectiveAuthority::root(&trust, true, None);

        let (execution, resolution) =
            root.constrain_policy_resolution(&trust, Tier::Four, [RiskKind::ProcessSpawn]);

        assert_eq!(execution, ExecutionPolicy::Controlled);
        assert_eq!(resolution.action, PolicyAction::Auto);
        assert_eq!(resolution.escalation, PolicyEscalation::Denied);
    }

    #[test]
    fn eager_deny_still_rejects_non_sandboxable_risk_and_authority_ceiling() {
        let trust = TrustConfig {
            mode: crate::trust::TrustMode::Eager,
            escalation: crate::trust::EscalationPolicy::Deny,
            ..TrustConfig::default()
        };
        let root = EffectiveAuthority::root(&trust, true, None);
        let (_, external) = root.constrain_policy_resolution(
            &trust,
            Tier::Four,
            [RiskKind::ProcessSpawn, RiskKind::WorkspaceExternal],
        );
        assert_eq!(external.action, PolicyAction::Deny);

        let constrained = EffectiveAuthority {
            tier_ceiling: [PolicyAction::Deny; 5],
            ..root
        };
        let (_, denied) =
            constrained.constrain_policy_resolution(&trust, Tier::Four, [RiskKind::ProcessSpawn]);
        assert_eq!(denied.action, PolicyAction::Deny);
    }

    #[test]
    fn inherited_child_authority_only_narrows_capabilities_and_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let parent_root = temp.path().join("parent");
        let child_root = parent_root.join("child");
        std::fs::create_dir_all(&child_root).unwrap();
        let parent =
            EffectiveAuthority::root(&TrustConfig::default(), true, Some(parent_root.clone()));

        let inherited = parent
            .inherited_child(true, ChildWorkspaceAuthority::Inherit)
            .unwrap();
        assert_eq!(
            inherited.workspace_root,
            Some(crate::fs_access::canonicalize_stable(&parent_root))
        );
        assert_eq!(inherited.allowed_tiers, parent.allowed_tiers);
        assert_eq!(inherited.allowed_risks, parent.allowed_risks);
        assert_eq!(inherited.shell, parent.shell);

        let narrowed = parent
            .inherited_child(false, ChildWorkspaceAuthority::Narrow(child_root.clone()))
            .unwrap();
        assert_eq!(
            narrowed.workspace_root,
            Some(crate::fs_access::canonicalize_stable(&child_root))
        );
        assert_eq!(narrowed.allowed_tiers, parent.allowed_tiers);
        assert_eq!(narrowed.allowed_risks, parent.allowed_risks);
        assert!(!narrowed.shell);
    }

    #[test]
    fn inherited_child_rejects_workspace_widening() {
        let temp = tempfile::tempdir().unwrap();
        let parent_root = temp.path().join("parent");
        std::fs::create_dir(&parent_root).unwrap();
        let parent =
            EffectiveAuthority::root(&TrustConfig::default(), true, Some(parent_root.clone()));

        let error = parent
            .inherited_child(
                true,
                ChildWorkspaceAuthority::Narrow(temp.path().join("sibling")),
            )
            .unwrap_err();

        assert_eq!(error, "child workspace must be within the parent workspace");
    }

    #[test]
    fn inherited_child_rejects_parent_traversal_for_nonexistent_target() {
        let temp = tempfile::tempdir().unwrap();
        let parent_root = temp.path().join("parent");
        std::fs::create_dir(&parent_root).unwrap();
        let parent =
            EffectiveAuthority::root(&TrustConfig::default(), true, Some(parent_root.clone()));

        let error = parent
            .inherited_child(
                true,
                ChildWorkspaceAuthority::Narrow(parent_root.join("missing/../../escape")),
            )
            .unwrap_err();

        assert_eq!(error, "child workspace must not contain parent traversal");
    }

    #[cfg(unix)]
    #[test]
    fn inherited_child_rejects_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let parent_root = temp.path().join("parent");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&parent_root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, parent_root.join("escape")).unwrap();
        let parent =
            EffectiveAuthority::root(&TrustConfig::default(), true, Some(parent_root.clone()));

        let error = parent
            .inherited_child(
                true,
                ChildWorkspaceAuthority::Narrow(parent_root.join("escape/missing")),
            )
            .unwrap_err();

        assert_eq!(error, "child workspace must be within the parent workspace");
    }

    #[test]
    fn trusted_workspace_delegation_can_replace_the_parent_root() {
        let temp = tempfile::tempdir().unwrap();
        let parent_root = temp.path().join("parent");
        let delegated_root = temp.path().join("managed-worktree");
        std::fs::create_dir(&parent_root).unwrap();
        std::fs::create_dir(&delegated_root).unwrap();
        let parent = EffectiveAuthority::root(&TrustConfig::default(), true, Some(parent_root));

        let child = parent
            .inherited_child(
                true,
                ChildWorkspaceAuthority::TrustedDelegation(delegated_root.clone()),
            )
            .unwrap();

        assert_eq!(
            child.workspace_root,
            Some(crate::fs_access::canonicalize_stable(&delegated_root))
        );
        assert_eq!(child.allowed_tiers, parent.allowed_tiers);
        assert_eq!(child.allowed_risks, parent.allowed_risks);
        assert_eq!(child.shell, parent.shell);
    }
}
