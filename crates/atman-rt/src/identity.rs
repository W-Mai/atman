use core::fmt;

use serde::{Deserialize, Serialize};

/// Supplies identity creation from the embedding host.
pub trait IdSource {
    fn fresh() -> Self;
}

/// Identity of one flow execution, independent of sessions and storage.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RunId<I>(pub I);

/// Identity of one turn, independent of its input source and storage.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TurnId<I>(pub I);

/// Persistent identity of a flow instance, distinct from each invocation's run ID.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FlowInstanceId<I>(pub I);

/// Address of an instance within a host's session namespace.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct FlowRef<I> {
    pub session_id: I,
    pub instance_id: FlowInstanceId<I>,
}

impl<I> FlowInstanceId<I> {
    pub const fn new(value: I) -> Self {
        Self(value)
    }
}

impl<I: IdSource> FlowInstanceId<I> {
    pub fn now() -> Self {
        Self(I::fresh())
    }
}

impl<I: fmt::Display> fmt::Display for FlowInstanceId<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<I> RunId<I> {
    pub const fn new(value: I) -> Self {
        Self(value)
    }
}

impl<I: IdSource> RunId<I> {
    pub fn now() -> Self {
        Self(I::fresh())
    }
}

impl<I> TurnId<I> {
    pub const fn new(value: I) -> Self {
        Self(value)
    }
}

impl<I: IdSource> TurnId<I> {
    pub fn now() -> Self {
        Self(I::fresh())
    }
}

impl<I: fmt::Display> fmt::Display for RunId<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<I: fmt::Display> fmt::Display for TurnId<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    struct Fixed(u8);

    impl IdSource for Fixed {
        fn fresh() -> Self {
            Self(7)
        }
    }

    #[test]
    fn host_supplies_flow_and_turn_identities() {
        assert_eq!(RunId::<Fixed>::now().0, Fixed(7));
        assert_eq!(TurnId::<Fixed>::now().0, Fixed(7));
    }
}
