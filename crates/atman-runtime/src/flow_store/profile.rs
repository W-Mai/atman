//! Profile-owned management identity and explicit grants, committed together.

use std::{collections::BTreeMap, fs::File, io, io::Write};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowRef, RuntimeStore, SessionConfiguration, SessionDescriptor, SessionProjection,
    access::{AccessTarget, FlowAction, FlowGrant, SessionAccess},
    initialize_dir, invalid,
};
use crate::event_log::journal::{CommitReceipt, Journal, RecoveredTail, private_file_options};

const VERSION: u32 = 1;

mod controls;
pub use controls::{
    ControlBatchItem, ControlBatchRecord, ControlBatchRequest, ControlBatchUpdate, ControlProgress,
    ControlProgressRecord,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagementBinding {
    pub session: SessionDescriptor,
    /// Explicit per-flow authority. Membership alone grants no cross-session access.
    pub grants: Vec<FlowGrant>,
}

impl ManagementBinding {
    pub(crate) fn allows_scope(
        &self,
        source: &FlowRef,
        target: &AccessTarget,
        action: FlowAction,
    ) -> bool {
        self.grants
            .iter()
            .any(|grant| grant.allows_scope(source, target, action))
    }
    fn validate(&self) -> io::Result<()> {
        self.session.validate()?;
        if self.session.id.is_nil() {
            return Err(invalid("management session identity must not be nil"));
        }
        let mut access = SessionAccess::default();
        for grant in &self.grants {
            let valid_target = match &grant.target {
                AccessTarget::Flow(flow) => {
                    !flow.session_id.0.is_nil() && !flow.instance_id.0.0.is_nil()
                }
                AccessTarget::Session(id) => !id.is_nil(),
                AccessTarget::AllSessions => true,
            };
            if grant.grantee.session_id.0 != self.session.id
                || grant.grantee.instance_id.0.0.is_nil()
                || !valid_target
            {
                return Err(invalid("management grant has an invalid source or target"));
            }
            access.grant(grant.clone())?;
        }
        Ok(())
    }

    pub fn allows(&self, source: &FlowRef, target: &FlowRef, action: FlowAction) -> bool {
        self.grants
            .iter()
            .any(|grant| grant.allows(source, target, action))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileSnapshot {
    pub profile_id: Uuid,
    pub cursor: CommitReceipt,
    pub management: Option<ManagementBinding>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReplaceManagement {
    pub request_id: Uuid,
    pub expected: CommitReceipt,
    pub binding: ManagementBinding,
}

/// One exclusive profile owner. Dropping the store releases ownership without changing any session or flow lifecycle.
pub struct ProfileStore {
    runtime: RuntimeStore,
    journal: Journal,
    projection: Projection,
    recovered_tail: Option<RecoveredTail>,
    write_failed: bool,
    _owner: File,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u32,
    profile_id: Uuid,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum Fact {
    Created,
    BootstrapPrepared(SessionDescriptor),
    BootstrapBound,
    ManagementReplaced(ReplaceManagement),
    ControlsSubmitted(ControlBatchRequest),
    ControlProgressed(ControlBatchUpdate),
}

#[derive(Clone)]
struct Projection {
    snapshot: ProfileSnapshot,
    prepared: Option<SessionDescriptor>,
    requests: BTreeMap<Uuid, (ReplaceManagement, CommitReceipt)>,
    controls: controls::ControlBatches,
}

impl Projection {
    fn replay(
        projection: &mut Option<Self>,
        profile_id: Uuid,
        receipt: CommitReceipt,
        facts: Vec<Fact>,
    ) -> io::Result<()> {
        if facts.len() != 1 {
            return Err(invalid("profile command must contain one fact"));
        }
        for fact in facts {
            match (projection.as_mut(), fact) {
                (None, Fact::Created) if receipt.sequence == 1 => {
                    *projection = Some(Self {
                        snapshot: ProfileSnapshot {
                            profile_id,
                            cursor: receipt.clone(),
                            management: None,
                        },
                        prepared: None,
                        requests: BTreeMap::new(),
                        controls: Default::default(),
                    });
                }
                (Some(state), fact) => state.apply(fact, receipt.clone())?,
                _ => return Err(invalid("profile creation was not recorded")),
            }
        }
        Ok(())
    }

    fn apply(&mut self, fact: Fact, receipt: CommitReceipt) -> io::Result<()> {
        match fact {
            Fact::ControlsSubmitted(request) => self.controls.submit(request, receipt.clone())?,
            Fact::ControlProgressed(update) => self.controls.advance(update, receipt.clone())?,
            Fact::BootstrapPrepared(descriptor)
                if self.prepared.is_none() && self.snapshot.management.is_none() =>
            {
                ManagementBinding {
                    session: descriptor.clone(),
                    grants: Vec::new(),
                }
                .validate()?;
                self.prepared = Some(descriptor);
            }
            Fact::BootstrapBound if self.snapshot.management.is_none() => {
                self.snapshot.management = Some(ManagementBinding {
                    session: self
                        .prepared
                        .take()
                        .ok_or_else(|| invalid("management bootstrap was not prepared"))?,
                    grants: Vec::new(),
                });
            }
            Fact::ManagementReplaced(request) if self.prepared.is_none() => {
                if request.request_id.is_nil()
                    || request.expected != self.snapshot.cursor
                    || self.requests.contains_key(&request.request_id)
                {
                    return Err(invalid("management request identity or revision changed"));
                }
                request.binding.validate()?;
                self.snapshot.management = Some(request.binding.clone());
                self.requests
                    .insert(request.request_id, (request, receipt.clone()));
            }
            _ => return Err(invalid("invalid profile transition")),
        }
        self.snapshot.cursor = receipt;
        Ok(())
    }
}

impl ProfileStore {
    /// Called under the catalog lock; binding changes use the same lock. Inspection never creates or repairs the profile.
    pub(super) fn protected_session(runtime: &RuntimeStore) -> io::Result<Option<Uuid>> {
        let directory = runtime.root.join("host");
        if !directory.try_exists()? {
            return Ok(None);
        }
        let identity = read_identity(&directory)?;
        let mut projection = None;
        let journal = Journal::inspect(
            &directory.join("events.jsonl"),
            identity.profile_id,
            |receipt, facts| {
                Projection::replay(&mut projection, identity.profile_id, receipt, facts)
            },
        )?;
        if journal.trailing_bytes != 0 {
            return Err(invalid("management profile journal is incomplete"));
        }
        let projection = projection.ok_or_else(|| invalid("profile creation was not recorded"))?;
        Ok(projection
            .snapshot
            .management
            .map(|binding| binding.session.id)
            .or_else(|| projection.prepared.map(|session| session.id)))
    }

    pub(crate) fn data_dir(&self) -> &std::path::Path {
        self.runtime.root.parent().unwrap()
    }
    /// Acquire profile ownership independently of session writers. An existing corrupt profile is never recreated.
    pub fn open(runtime: &RuntimeStore) -> io::Result<Self> {
        let owner = private_file_options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(runtime.root.join(".profile.lock"))?;
        fs2::FileExt::try_lock_exclusive(&owner)?;
        let directory = runtime.root.join("host");
        if !directory.try_exists()? {
            initialize_dir(&directory, |dir| {
                let identity = Identity {
                    version: VERSION,
                    profile_id: Uuid::now_v7(),
                };
                let mut file = private_file_options()
                    .create_new(true)
                    .write(true)
                    .open(dir.join("identity.json"))?;
                file.write_all(&serde_json::to_vec(&identity).map_err(invalid)?)?;
                file.sync_all()?;
                Journal::create(&dir.join("events.jsonl"), identity.profile_id)?
                    .commit(&[Fact::Created])?;
                Ok(())
            })?;
        }
        let identity = read_identity(&directory)?;
        let mut projection = None;
        let (journal, recovered_tail) = Journal::open(
            &directory.join("events.jsonl"),
            identity.profile_id,
            |receipt, facts| {
                Projection::replay(&mut projection, identity.profile_id, receipt, facts)
            },
        )?;
        Ok(Self {
            runtime: RuntimeStore {
                root: runtime.root.clone(),
            },
            journal,
            projection: projection.ok_or_else(|| invalid("profile creation was not recorded"))?,
            recovered_tail,
            write_failed: false,
            _owner: owner,
        })
    }

    pub fn snapshot(&self) -> &ProfileSnapshot {
        &self.projection.snapshot
    }

    pub fn recovered_tail(&self) -> Option<&RecoveredTail> {
        self.recovered_tail.as_ref()
    }

    pub(crate) fn validate_management(&self) -> io::Result<()> {
        if self.write_failed {
            return Err(io::Error::other(
                "profile write failed; reopen to reconcile the journal",
            ));
        }
        if let Some(binding) = &self.snapshot().management {
            self.inspect_binding(binding)?;
        }
        Ok(())
    }

    pub(crate) fn write_failed(&self) -> bool {
        self.write_failed
    }

    /// Create the projectless container once, without opening flows or installing scheduling. Configuration is used only for its initial descriptor.
    pub fn bootstrap(
        &mut self,
        configuration: SessionConfiguration,
    ) -> io::Result<ProfileSnapshot> {
        let _catalog = self.runtime.lock_catalog()?;
        self.validate_management()?;
        if self.snapshot().management.is_some() {
            return Ok(self.snapshot().clone());
        }
        if self.projection.prepared.is_none() {
            self.commit(Fact::BootstrapPrepared(SessionDescriptor {
                id: Uuid::now_v7(),
                project: None,
                configuration,
                title: "Atman".into(),
            }))?;
        }
        let descriptor = self.projection.prepared.as_ref().unwrap().clone();
        match self.runtime.create_session_locked(descriptor.clone()) {
            Ok(store) => drop(store),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                self.inspect_binding(&ManagementBinding {
                    session: descriptor,
                    grants: Vec::new(),
                })?;
            }
            Err(error) => return Err(error),
        }
        self.commit(Fact::BootstrapBound)?;
        Ok(self.snapshot().clone())
    }

    /// Replace the binding and its grants atomically. A retry returns the original receipt; it never reinstalls an older binding.
    pub fn replace(&mut self, request: ReplaceManagement) -> io::Result<CommitReceipt> {
        if let Some((previous, receipt)) = self.projection.requests.get(&request.request_id) {
            return if previous == &request {
                Ok(receipt.clone())
            } else {
                Err(invalid(
                    "management request identity conflicts with recorded content",
                ))
            };
        }
        let _catalog = self.runtime.lock_catalog()?;
        self.inspect_binding(&request.binding)?;
        self.commit(Fact::ManagementReplaced(request))
    }

    /// Resolve a selected Session through its retained catalog. Retries retain the original descriptor even if that Session later becomes unavailable.
    pub fn select(
        &mut self,
        request_id: Uuid,
        expected: CommitReceipt,
        session_id: Uuid,
        grants: Vec<FlowGrant>,
    ) -> io::Result<CommitReceipt> {
        let session = if let Some((recorded, _)) = self.projection.requests.get(&request_id) {
            if recorded.binding.session.id != session_id {
                return Err(invalid(
                    "management request identity conflicts with recorded content",
                ));
            }
            recorded.binding.session.clone()
        } else {
            RuntimeStore::inspect_session(self.runtime.root.parent().unwrap(), session_id)?
                .projection
                .descriptor
        };
        self.replace(ReplaceManagement {
            request_id,
            expected,
            binding: ManagementBinding { session, grants },
        })
    }

    fn inspect_binding(&self, binding: &ManagementBinding) -> io::Result<()> {
        binding.validate()?;
        let catalog =
            RuntimeStore::inspect_session(self.runtime.root.parent().unwrap(), binding.session.id)?;
        if catalog.projection.descriptor != binding.session || catalog.journal.trailing_bytes != 0 {
            return Err(invalid(
                "management session descriptor or journal is incomplete",
            ));
        }
        let SessionProjection { flows, .. } = catalog.projection;
        if binding.grants.iter().any(|grant| {
            !flows
                .iter()
                .any(|flow| flow.id == grant.grantee.instance_id)
        }) {
            return Err(invalid(
                "management grant source is not registered in its session",
            ));
        }
        Ok(())
    }

    fn commit(&mut self, fact: Fact) -> io::Result<CommitReceipt> {
        let mut candidate = self.projection.clone();
        // Validate before writing. The real receipt is assigned only after synchronization.
        candidate.apply(fact.clone(), self.snapshot().cursor.clone())?;
        let receipt = self
            .journal
            .commit(std::slice::from_ref(&fact))
            .inspect_err(|_| {
                self.write_failed = true;
            })?;
        self.projection.apply(fact, receipt.clone())?;
        Ok(receipt)
    }
}

fn read_identity(directory: &std::path::Path) -> io::Result<Identity> {
    let identity: Identity =
        serde_json::from_slice(&std::fs::read(directory.join("identity.json"))?)
            .map_err(invalid)?;
    if identity.version != VERSION || identity.profile_id.is_nil() {
        return Err(invalid("unsupported profile version or invalid identity"));
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_bootstrap_recovers_the_same_container_before_and_after_creation() {
        for already_created in [false, true] {
            let data = tempfile::tempdir().unwrap();
            let root = RuntimeStore::open(data.path()).unwrap();
            let mut profile = ProfileStore::open(&root).unwrap();
            let descriptor = SessionDescriptor {
                id: Uuid::now_v7(),
                project: None,
                configuration: Default::default(),
                title: "Atman".into(),
            };
            profile
                .commit(Fact::BootstrapPrepared(descriptor.clone()))
                .unwrap();
            if already_created {
                root.create_session(descriptor.clone()).unwrap();
            }
            drop(profile);
            let mut profile = ProfileStore::open(&root).unwrap();
            assert_eq!(
                profile
                    .bootstrap(Default::default())
                    .unwrap()
                    .management
                    .unwrap()
                    .session,
                descriptor
            );
            assert_eq!(
                RuntimeStore::session_ids(data.path()).unwrap(),
                vec![descriptor.id]
            );
        }
    }
}
