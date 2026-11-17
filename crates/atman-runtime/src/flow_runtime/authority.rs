//! Native writer fencing shared with the internal peer effect executor.

use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{RuntimeHost, SessionAdmission};
use crate::flow_runtime::task_scope::ScopeAdmission;
use crate::{
    flow_store::invalid,
    peer_authority::{AuthorityGuard, AuthorityKey, AuthorityMode, AuthorityPlan, PeerAuthority},
};

impl RuntimeHost {
    /// Bind only the trusted canonical storage domain; this does not acquire any execution owner.
    pub fn bind_peer_authority(&self, authority: PeerAuthority) -> io::Result<()> {
        if let Some(current) = self.directory.authority.get() {
            return same_domain(current, &authority);
        }
        let _sessions = self.directory.sessions.lock().unwrap();
        for root in self.directory.storage_roots.lock().unwrap().values() {
            if root != authority.data_root() {
                return Err(invalid("peer authority belongs to another storage root"));
            }
        }
        match self.directory.authority.set(authority) {
            Ok(()) => Ok(()),
            Err(authority) => same_domain(self.directory.authority.get().unwrap(), &authority),
        }
    }

    pub fn authority_domain(&self) -> Option<PeerAuthority> {
        self.directory.authority.get().cloned()
    }

    pub fn peer_authority(&self, session_id: Uuid) -> io::Result<PeerAuthority> {
        if session_id.is_nil() {
            return Err(invalid("peer authority Session identity is nil"));
        }
        self.authority_domain()
            .ok_or_else(|| invalid("peer authority domain is not installed"))
    }

    pub fn set_peer_port(
        &self,
        port: Arc<dyn crate::flow_runtime::peer::PeerPort>,
    ) -> io::Result<()> {
        self.directory
            .peers
            .set(port)
            .map_err(|_| invalid("internal peer port is already installed"))
    }

    pub fn peer_port(&self) -> Option<Arc<dyn crate::flow_runtime::peer::PeerPort>> {
        self.directory.peers.get().cloned()
    }

    pub(in crate::flow_runtime) fn invocation_registry(
        &self,
    ) -> &crate::flow_runtime::peer::InvocationRegistry {
        &self.directory.invocations
    }
}

fn same_domain(left: &PeerAuthority, right: &PeerAuthority) -> io::Result<()> {
    if left.profile_id() == right.profile_id() && left.data_root() == right.data_root() {
        Ok(())
    } else {
        Err(invalid("peer authority domain is already bound"))
    }
}

impl SessionAdmission {
    pub(in crate::flow_runtime) async fn prepare_authority(
        &self,
        sources: ScopeAdmission,
        prepared: Option<Arc<AuthorityGuard>>,
    ) -> io::Result<Option<Arc<AuthorityGuard>>> {
        let Some(authority) = self.authority.get() else {
            return if prepared.is_some() {
                Err(invalid("prepared authority has no original storage domain"))
            } else {
                Ok(None)
            };
        };
        let session = self
            .identity
            .as_ref()
            .ok_or_else(|| invalid("authority requires an owned Session"))?
            .session_id;
        let mut requirements = sources.authority_requirements();
        requirements.push((AuthorityKey::Session(session), AuthorityMode::Exclusive));
        let plan = AuthorityPlan::new(requirements)?;
        if let Some(prepared) = prepared {
            if !prepared.belongs_to(authority)
                || plan
                    .participants()
                    .iter()
                    .any(|(key, mode)| !prepared.covers(*key, *mode))
            {
                return Err(invalid(
                    "prepared authority does not cover this writer transaction",
                ));
            }
            return Ok(Some(prepared));
        }
        authority
            .acquire(
                plan,
                CancellationToken::new(),
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .map(|guard| Some(Arc::new(guard)))
    }

    pub(in crate::flow_runtime) fn check_read_authority(
        &self,
        prepared: &AuthorityGuard,
    ) -> io::Result<()> {
        let authority = self
            .authority
            .get()
            .ok_or_else(|| invalid("peer authority domain is not installed"))?;
        let session = self
            .identity
            .as_ref()
            .ok_or_else(|| invalid("authority requires an owned Session"))?
            .session_id;
        if !prepared.belongs_to(authority)
            || !prepared.covers(AuthorityKey::Session(session), AuthorityMode::Shared)
        {
            return Err(invalid("prepared authority does not cover this read"));
        }
        Ok(())
    }
}
