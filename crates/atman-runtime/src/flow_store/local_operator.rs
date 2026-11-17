//! Trusted local operator bindings with retained request namespaces.

use std::io;

use uuid::Uuid;

use super::{inbox::SenderIdentity, invalid};

/// A host-authenticated operator, independent of a request's serialized sender.
///
/// Native and legacy RPC bindings can reconcile each other's original receipts. Every other operator remains restricted to its own namespace. This type is intentionally not deserializable and does not confer authorization by itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalOperator(Uuid);

impl LocalOperator {
    pub const NATIVE: Self = Self(Uuid::from_bytes(*b"atman-local-user"));
    pub const LEGACY_RPC: Self = Self(Uuid::from_bytes(*b"atman-operator!!"));

    /// Bind an operator authenticated by the host, never a caller-selected sender.
    pub const fn new(id: Uuid) -> Self {
        Self(id)
    }

    pub const fn id(self) -> Uuid {
        self.0
    }

    pub fn sender(self) -> SenderIdentity {
        SenderIdentity::User(self.0)
    }

    /// Test namespace ownership without granting access to Flow or Host senders.
    pub fn owns(self, sender: &SenderIdentity) -> bool {
        let SenderIdentity::User(id) = sender else {
            return false;
        };
        *id == self.0
            || (self == Self::NATIVE || self == Self::LEGACY_RPC)
                && (*id == Self::NATIVE.0 || *id == Self::LEGACY_RPC.0)
    }

    pub(crate) fn resolve(
        self,
        requested: Option<&SenderIdentity>,
        mut exists: impl FnMut(&SenderIdentity) -> bool,
    ) -> io::Result<SenderIdentity> {
        if requested.is_some_and(|sender| !self.owns(sender)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "request sender does not belong to the authenticated operator",
            ));
        }
        let alternate = if self == Self::NATIVE {
            Some(Self::LEGACY_RPC)
        } else if self == Self::LEGACY_RPC {
            Some(Self::NATIVE)
        } else {
            None
        };
        let mut origin = None;
        for candidate in [Some(self), alternate].into_iter().flatten() {
            let sender = candidate.sender();
            if exists(&sender) {
                if origin.is_some() {
                    return Err(invalid(
                        "operator request has ambiguous original namespaces",
                    ));
                }
                origin = Some(sender);
            }
        }
        Ok(origin.unwrap_or_else(|| requested.cloned().unwrap_or_else(|| self.sender())))
    }
}

#[cfg(test)]
mod tests;
