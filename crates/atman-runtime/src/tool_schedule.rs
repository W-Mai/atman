use std::path::{Path, PathBuf};

/// A normalized runtime resource used to order tool calls.
///
/// Path-bearing keys must be resolved and normalized by the claim producer before they reach the
/// scheduler. The scheduler only applies component-aware containment rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    Path(PathBuf),
    TerminalHandle(String),
    ProcessHandle(String),
    Repository(PathBuf),
    Session(String),
    McpServer(String),
    Named(String),
}

impl ResourceKey {
    /// Returns whether two keys describe overlapping runtime resources.
    pub fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Path(left), Self::Path(right))
            | (Self::Path(left), Self::Repository(right))
            | (Self::Repository(left), Self::Path(right))
            | (Self::Repository(left), Self::Repository(right)) => paths_overlap(left, right),
            _ => self == other,
        }
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessMode {
    Read,
    Write,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceAccess {
    pub resource: ResourceKey,
    pub mode: AccessMode,
}

impl ResourceAccess {
    pub fn new(resource: ResourceKey, mode: AccessMode) -> Self {
        Self { resource, mode }
    }

    pub fn read(resource: ResourceKey) -> Self {
        Self::new(resource, AccessMode::Read)
    }

    pub fn write(resource: ResourceKey) -> Self {
        Self::new(resource, AccessMode::Write)
    }

    pub fn exclusive(resource: ResourceKey) -> Self {
        Self::new(resource, AccessMode::Exclusive)
    }

    /// Returns whether these accesses must retain their source order.
    pub fn conflicts_with(&self, other: &Self) -> bool {
        self.resource.overlaps(&other.resource)
            && !matches!(
                (self.mode, other.mode),
                (AccessMode::Read, AccessMode::Read)
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SchedulingPolicy {
    Declared,
    Opaque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulingClaims {
    pub accesses: Vec<ResourceAccess>,
    pub policy: SchedulingPolicy,
}

impl SchedulingClaims {
    pub fn declared(accesses: impl IntoIterator<Item = ResourceAccess>) -> Self {
        Self {
            accesses: accesses.into_iter().collect(),
            policy: SchedulingPolicy::Declared,
        }
    }

    /// Creates a bidirectional scheduling barrier.
    pub fn opaque() -> Self {
        Self {
            accesses: Vec::new(),
            policy: SchedulingPolicy::Opaque,
        }
    }

    pub fn is_opaque(&self) -> bool {
        self.policy == SchedulingPolicy::Opaque
    }

    /// Returns whether two calls require a source-order edge.
    pub fn conflicts_with(&self, other: &Self) -> bool {
        self.is_opaque()
            || other.is_opaque()
            || self.accesses.iter().any(|left| {
                other
                    .accesses
                    .iter()
                    .any(|right| left.conflicts_with(right))
            })
    }
}

impl Default for SchedulingClaims {
    fn default() -> Self {
        Self::opaque()
    }
}

/// Current lifecycle state for one node in a scheduling graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleNodeState {
    Pending,
    Running,
    Completed,
}

/// A stable conflict DAG together with its execution state.
///
/// Edges always point from a lower source index to a higher source index. `ready_wave` therefore
/// returns pending nodes in source order while still allowing each completed node to release its
/// successors independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchedulingGraph {
    successors: Vec<Vec<usize>>,
    predecessor_counts: Vec<usize>,
    remaining_predecessors: Vec<usize>,
    states: Vec<ScheduleNodeState>,
}

impl SchedulingGraph {
    pub fn from_claims(claims: &[SchedulingClaims]) -> Self {
        let mut successors = vec![Vec::new(); claims.len()];
        let mut predecessor_counts = vec![0; claims.len()];

        for earlier in 0..claims.len() {
            for later in (earlier + 1)..claims.len() {
                if claims[earlier].conflicts_with(&claims[later]) {
                    successors[earlier].push(later);
                    predecessor_counts[later] += 1;
                }
            }
        }

        Self {
            successors,
            remaining_predecessors: predecessor_counts.clone(),
            predecessor_counts,
            states: vec![ScheduleNodeState::Pending; claims.len()],
        }
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn is_complete(&self) -> bool {
        self.states
            .iter()
            .all(|state| *state == ScheduleNodeState::Completed)
    }

    /// Returns all pending zero-indegree nodes in source order.
    pub fn ready_wave(&self) -> Vec<usize> {
        self.states
            .iter()
            .zip(&self.remaining_predecessors)
            .enumerate()
            .filter_map(|(index, (state, remaining))| {
                (*state == ScheduleNodeState::Pending && *remaining == 0).then_some(index)
            })
            .collect()
    }

    /// Marks a ready node as running.
    pub fn start(&mut self, index: usize) -> Result<(), SchedulingGraphError> {
        let state = self.state_at(index)?;
        match state {
            ScheduleNodeState::Pending if self.remaining_predecessors[index] == 0 => {
                self.states[index] = ScheduleNodeState::Running;
                Ok(())
            }
            ScheduleNodeState::Pending => Err(SchedulingGraphError::NotReady { index }),
            ScheduleNodeState::Running => Err(SchedulingGraphError::AlreadyRunning { index }),
            ScheduleNodeState::Completed => Err(SchedulingGraphError::AlreadyCompleted { index }),
        }
    }

    /// Completes a running node and releases each outgoing ordering edge.
    pub fn complete(&mut self, index: usize) -> Result<(), SchedulingGraphError> {
        match self.state_at(index)? {
            ScheduleNodeState::Running => {}
            ScheduleNodeState::Pending => return Err(SchedulingGraphError::NotRunning { index }),
            ScheduleNodeState::Completed => {
                return Err(SchedulingGraphError::AlreadyCompleted { index });
            }
        }

        self.release(index);
        Ok(())
    }

    /// Settles a ready node that will not execute, such as a rejected call.
    pub fn settle_ready(&mut self, index: usize) -> Result<(), SchedulingGraphError> {
        match self.state_at(index)? {
            ScheduleNodeState::Pending if self.remaining_predecessors[index] == 0 => {}
            ScheduleNodeState::Pending => return Err(SchedulingGraphError::NotReady { index }),
            ScheduleNodeState::Running => {
                return Err(SchedulingGraphError::AlreadyRunning { index });
            }
            ScheduleNodeState::Completed => {
                return Err(SchedulingGraphError::AlreadyCompleted { index });
            }
        }

        self.release(index);
        Ok(())
    }

    fn release(&mut self, index: usize) {
        self.states[index] = ScheduleNodeState::Completed;
        for &successor in &self.successors[index] {
            debug_assert!(self.remaining_predecessors[successor] > 0);
            self.remaining_predecessors[successor] -= 1;
        }
    }

    pub fn node_state(&self, index: usize) -> Option<ScheduleNodeState> {
        self.states.get(index).copied()
    }

    pub fn successors(&self, index: usize) -> Option<&[usize]> {
        self.successors.get(index).map(Vec::as_slice)
    }

    pub fn predecessor_count(&self, index: usize) -> Option<usize> {
        self.predecessor_counts.get(index).copied()
    }

    pub fn remaining_predecessor_count(&self, index: usize) -> Option<usize> {
        self.remaining_predecessors.get(index).copied()
    }

    fn state_at(&self, index: usize) -> Result<ScheduleNodeState, SchedulingGraphError> {
        self.states
            .get(index)
            .copied()
            .ok_or(SchedulingGraphError::OutOfBounds {
                index,
                len: self.len(),
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SchedulingGraphError {
    #[error("scheduling node {index} is out of bounds for graph of length {len}")]
    OutOfBounds { index: usize, len: usize },
    #[error("scheduling node {index} still has predecessors")]
    NotReady { index: usize },
    #[error("scheduling node {index} is not running")]
    NotRunning { index: usize },
    #[error("scheduling node {index} is already running")]
    AlreadyRunning { index: usize },
    #[error("scheduling node {index} is already completed")]
    AlreadyCompleted { index: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> ResourceKey {
        ResourceKey::Named(name.to_owned())
    }

    fn read(resource: ResourceKey) -> SchedulingClaims {
        SchedulingClaims::declared([ResourceAccess::read(resource)])
    }

    fn write(resource: ResourceKey) -> SchedulingClaims {
        SchedulingClaims::declared([ResourceAccess::write(resource)])
    }

    fn start_and_complete(graph: &mut SchedulingGraph, wave: &[usize]) {
        for &index in wave {
            graph.start(index).unwrap();
        }
        for &index in wave {
            graph.complete(index).unwrap();
        }
    }

    #[test]
    fn opaque_is_the_safe_default_and_a_bidirectional_conflict() {
        let opaque = SchedulingClaims::default();
        let declared_empty = SchedulingClaims::declared([]);

        assert_eq!(opaque, SchedulingClaims::opaque());
        assert!(opaque.conflicts_with(&declared_empty));
        assert!(declared_empty.conflicts_with(&opaque));
        assert!(!declared_empty.conflicts_with(&declared_empty));
    }

    #[test]
    fn only_two_reads_may_overlap_on_the_same_resource() {
        let key = named("shared");
        let accesses = [
            ResourceAccess::read(key.clone()),
            ResourceAccess::write(key.clone()),
            ResourceAccess::exclusive(key),
        ];

        for (left_index, left) in accesses.iter().enumerate() {
            for (right_index, right) in accesses.iter().enumerate() {
                assert_eq!(
                    left.conflicts_with(right),
                    !(left_index == 0 && right_index == 0)
                );
            }
        }
    }

    #[test]
    fn path_overlap_is_component_aware() {
        let root = ResourceKey::Path(PathBuf::from("/workspace/repo"));
        let child = ResourceKey::Path(PathBuf::from("/workspace/repo/src/lib.rs"));
        let string_prefix_sibling = ResourceKey::Path(PathBuf::from("/workspace/repository/file"));

        assert!(root.overlaps(&child));
        assert!(child.overlaps(&root));
        assert!(!root.overlaps(&string_prefix_sibling));
    }

    #[test]
    fn repository_and_contained_path_conflict_across_variants() {
        let repository = ResourceKey::Repository(PathBuf::from("/workspace/repo"));
        let contained = ResourceKey::Path(PathBuf::from("/workspace/repo/src/lib.rs"));
        let outside = ResourceKey::Path(PathBuf::from("/workspace/other/src/lib.rs"));

        assert!(repository.overlaps(&contained));
        assert!(contained.overlaps(&repository));
        assert!(!repository.overlaps(&outside));
        assert!(
            !ResourceAccess::read(repository.clone())
                .conflicts_with(&ResourceAccess::read(contained.clone()))
        );
        assert!(
            ResourceAccess::read(repository.clone())
                .conflicts_with(&ResourceAccess::write(contained.clone()))
        );
        assert!(
            ResourceAccess::exclusive(repository).conflicts_with(&ResourceAccess::read(contained))
        );
    }

    #[test]
    fn independent_nodes_share_the_first_stable_ready_wave() {
        let claims = [read(named("b")), read(named("a")), read(named("c"))];
        let graph = SchedulingGraph::from_claims(&claims);

        assert_eq!(graph.ready_wave(), vec![0, 1, 2]);
        assert_eq!(graph.successors(0), Some([].as_slice()));
    }

    #[test]
    fn write_then_reads_releases_both_readers_together() {
        let key = named("resource");
        let claims = [write(key.clone()), read(key.clone()), read(key)];
        let mut graph = SchedulingGraph::from_claims(&claims);

        assert_eq!(graph.ready_wave(), vec![0]);
        start_and_complete(&mut graph, &[0]);
        assert_eq!(graph.ready_wave(), vec![1, 2]);
        assert_eq!(graph.predecessor_count(2), Some(1));
    }

    #[test]
    fn read_write_read_forms_a_full_source_order_chain() {
        let key = named("resource");
        let claims = [read(key.clone()), write(key.clone()), read(key)];
        let mut graph = SchedulingGraph::from_claims(&claims);

        assert_eq!(graph.successors(0), Some([1].as_slice()));
        assert_eq!(graph.successors(1), Some([2].as_slice()));
        assert_eq!(graph.ready_wave(), vec![0]);
        start_and_complete(&mut graph, &[0]);
        assert_eq!(graph.ready_wave(), vec![1]);
        start_and_complete(&mut graph, &[1]);
        assert_eq!(graph.ready_wave(), vec![2]);
    }

    #[test]
    fn opaque_waits_for_all_earlier_nodes_and_blocks_all_later_nodes() {
        let claims = [
            read(named("earlier-a")),
            read(named("earlier-b")),
            SchedulingClaims::opaque(),
            read(named("later-a")),
            read(named("later-b")),
        ];
        let mut graph = SchedulingGraph::from_claims(&claims);

        assert_eq!(graph.ready_wave(), vec![0, 1]);
        graph.start(0).unwrap();
        graph.start(1).unwrap();
        graph.complete(0).unwrap();
        assert!(graph.ready_wave().is_empty());
        graph.complete(1).unwrap();
        assert_eq!(graph.ready_wave(), vec![2]);
        start_and_complete(&mut graph, &[2]);
        assert_eq!(graph.ready_wave(), vec![3, 4]);
    }

    #[test]
    fn graph_state_rejects_starting_a_blocked_node() {
        let key = named("resource");
        let claims = [write(key.clone()), read(key)];
        let mut graph = SchedulingGraph::from_claims(&claims);

        assert_eq!(
            graph.start(1),
            Err(SchedulingGraphError::NotReady { index: 1 })
        );
        start_and_complete(&mut graph, &[0]);
        assert_eq!(graph.ready_wave(), vec![1]);
    }

    #[test]
    fn settled_rejection_releases_ordering_edges_without_execution() {
        let key = named("resource");
        let claims = [write(key.clone()), read(key)];
        let mut graph = SchedulingGraph::from_claims(&claims);

        graph.settle_ready(0).unwrap();

        assert_eq!(graph.node_state(0), Some(ScheduleNodeState::Completed));
        assert_eq!(graph.ready_wave(), vec![1]);
    }
}
