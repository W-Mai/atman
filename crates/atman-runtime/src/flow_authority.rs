use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::event::FlowRunId;
use crate::tool::Tier;
use crate::trust::{ExecutionPolicy, PolicyAction, RiskKind, TrustConfig};

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
    pub fn root(trust: &TrustConfig, shell: bool, workspace_root: Option<PathBuf>) -> Self {
        let execution_policy = trust.execution_policy();
        let allowed_tiers =
            [Tier::Zero, Tier::One, Tier::Two, Tier::Three, Tier::Four].map(|tier| {
                execution_policy == ExecutionPolicy::Unrestricted
                    || trust.resolve_tier(tier) != PolicyAction::Deny
            });
        let risks = all_risks();
        let allowed_risks = risks
            .into_iter()
            .filter(|risk| {
                execution_policy == ExecutionPolicy::Unrestricted
                    || trust.resolve_risk(*risk) != PolicyAction::Deny
            })
            .collect();
        let tier_ceiling = [Tier::Zero, Tier::One, Tier::Two, Tier::Three, Tier::Four]
            .map(|tier| trust.resolve_policy(tier, []));
        let risk_ceiling = risks.map(|risk| trust.resolve_policy(Tier::Zero, [risk]));
        Self {
            execution_policy,
            allowed_tiers,
            allowed_risks,
            tier_ceiling,
            risk_ceiling,
            shell,
            permission_management: false,
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
        let current_execution = match (self.execution_policy, trust.execution_policy()) {
            (ExecutionPolicy::Unrestricted, ExecutionPolicy::Unrestricted) => {
                ExecutionPolicy::Unrestricted
            }
            _ => ExecutionPolicy::Controlled,
        };
        if current_execution == ExecutionPolicy::Unrestricted {
            return (current_execution, PolicyAction::Auto);
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
        (
            current_execution,
            trust.resolve_policy(tier, risks).most_restrictive(ceiling),
        )
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
