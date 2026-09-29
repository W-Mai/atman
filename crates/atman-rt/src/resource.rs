//! Typed generational handles for host-owned resources.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use async_lock::{RwLock, RwLockReadGuardArc, RwLockWriteGuardArc};
use core::{
    any::{Any, TypeId},
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
};

use crate::{HostPayload, HostValueOps};

#[cfg(target_has_atomic = "ptr")]
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_has_atomic = "ptr")]
static NEXT_REGISTRY_ID: AtomicUsize = AtomicUsize::new(1);

/// Identifies one resource registry within the current process.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RegistryId(usize);

impl RegistryId {
    /// Creates an identity whose uniqueness is guaranteed by the caller.
    ///
    /// The host must not reuse `host_id` while another registry with the same identity can still
    /// be observed.
    pub const fn from_host_id(host_id: usize) -> Self {
        Self(host_id)
    }

    /// Allocates a process-unique identity without wrapping the identity counter.
    #[cfg(target_has_atomic = "ptr")]
    pub fn fresh() -> Result<Self, ResourceError> {
        NEXT_REGISTRY_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map(Self)
            .map_err(|_| ResourceError::IdentityExhausted)
    }
}

/// Opaque process-local identity of a Rust resource type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResourceTypeId(TypeId);

impl ResourceTypeId {
    /// Returns the process-local identity of `T`.
    pub fn of<T: 'static>() -> Self {
        Self(TypeId::of::<T>())
    }
}

/// Supplies nominal and process-local type identity for a host resource.
pub trait ResourceType: Sized + 'static {
    /// Stable name used for catalogs and diagnostics.
    const TYPE_NAME: &'static str;

    /// Process-local identity used to validate handles.
    fn type_id() -> ResourceTypeId {
        ResourceTypeId::of::<Self>()
    }
}

/// Host payloads that can carry an ephemeral resource handle.
pub trait ResourcePayload: HostValueOps {
    fn from_resource(handle: ErasedHandle) -> Self;

    fn as_resource(&self) -> Option<&ErasedHandle>;
}

/// Adds resource handles to an existing host payload without changing `Value`.
#[derive(Debug, Clone)]
pub enum WithResources<P> {
    Resource(ErasedHandle),
    Custom(P),
}

impl<P: HostPayload> HostPayload for WithResources<P> {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::Resource(_) => "resource",
            Self::Custom(payload) => payload.kind_name(),
        }
    }
}

impl<P: HostValueOps> HostValueOps for WithResources<P> {
    fn additive_text(&self) -> Option<String> {
        match self {
            Self::Resource(_) => None,
            Self::Custom(payload) => payload.additive_text(),
        }
    }

    fn equals(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Resource(left), Self::Resource(right)) => left == right,
            (Self::Custom(left), Self::Custom(right)) => left.equals(right),
            _ => false,
        }
    }

    fn add_expected() -> &'static str {
        P::add_expected()
    }
}

impl<P: HostValueOps> ResourcePayload for WithResources<P> {
    fn from_resource(handle: ErasedHandle) -> Self {
        Self::Resource(handle)
    }

    fn as_resource(&self) -> Option<&ErasedHandle> {
        match self {
            Self::Resource(handle) => Some(handle),
            Self::Custom(_) => None,
        }
    }
}

/// Untyped identity of one live or formerly live registry slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ErasedHandle {
    registry: RegistryId,
    slot: u32,
    generation: u32,
    type_id: ResourceTypeId,
}

impl ErasedHandle {
    /// Returns the registry that issued this handle.
    pub const fn registry_id(self) -> RegistryId {
        self.registry
    }

    /// Returns the stable slot index within the issuing registry.
    pub const fn slot(self) -> u32 {
        self.slot
    }

    /// Returns the slot generation observed when this handle was issued.
    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// Returns the process-local resource type identity carried by this handle.
    pub const fn resource_type_id(self) -> ResourceTypeId {
        self.type_id
    }

    /// Validates the carried type identity and creates a typed handle.
    pub fn typed<T: ResourceType>(self) -> Result<Handle<T>, ResourceError> {
        Handle::from_erased(self)
    }
}

/// Typed identity of one resource registry slot.
pub struct Handle<T: ResourceType> {
    raw: ErasedHandle,
    marker: PhantomData<fn() -> T>,
}

impl<T: ResourceType> Handle<T> {
    /// Validates the erased handle's type identity.
    pub fn from_erased(raw: ErasedHandle) -> Result<Self, ResourceError> {
        if raw.type_id != T::type_id() {
            return Err(ResourceError::ResourceTypeMismatch);
        }
        Ok(Self {
            raw,
            marker: PhantomData,
        })
    }

    /// Returns the underlying untyped handle.
    pub const fn erased(self) -> ErasedHandle {
        self.raw
    }

    /// Borrows the underlying untyped handle.
    pub const fn as_erased(&self) -> &ErasedHandle {
        &self.raw
    }
}

impl<T: ResourceType> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ResourceType> Copy for Handle<T> {}

impl<T: ResourceType> fmt::Debug for Handle<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Handle")
            .field("type_name", &T::TYPE_NAME)
            .field("raw", &self.raw)
            .finish()
    }
}

impl<T: ResourceType> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T: ResourceType> Eq for Handle<T> {}

impl<T: ResourceType> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T: ResourceType> From<Handle<T>> for ErasedHandle {
    fn from(handle: Handle<T>) -> Self {
        handle.raw
    }
}

impl<T: ResourceType> TryFrom<ErasedHandle> for Handle<T> {
    type Error = ResourceError;

    fn try_from(handle: ErasedHandle) -> Result<Self, Self::Error> {
        Self::from_erased(handle)
    }
}

/// Failure while creating, resolving, or releasing a resource handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceError {
    /// The process-wide registry identity counter cannot advance without wrapping.
    IdentityExhausted,
    /// No additional slot can be represented by a `u32` handle index.
    CapacityExhausted,
    /// The registry's short-lived slot table lock is currently held exclusively.
    RegistryBusy,
    /// The handle belongs to another registry.
    ForeignResource,
    /// The slot is absent, vacant, retired, or now has another generation.
    StaleResource,
    /// The handle, slot metadata, or stored Rust value has another resource type.
    ResourceTypeMismatch,
    /// The slot is currently borrowed or being released.
    ResourceBusy,
}

impl fmt::Display for ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::IdentityExhausted => "resource registry identity space is exhausted",
            Self::CapacityExhausted => "resource registry slot space is exhausted",
            Self::RegistryBusy => "resource registry is busy",
            Self::ForeignResource => "resource handle belongs to another registry",
            Self::StaleResource => "resource handle is stale",
            Self::ResourceTypeMismatch => "resource handle type does not match the stored value",
            Self::ResourceBusy => "resource is busy",
        })
    }
}

impl core::error::Error for ResourceError {}

struct SlotState {
    registry: RegistryId,
    slot: u32,
    generation: u32,
    type_id: Option<ResourceTypeId>,
    value: Option<Box<dyn Any + Send + Sync>>,
    retired: bool,
}

struct RegistryState {
    slots: Vec<Arc<RwLock<SlotState>>>,
    free: Vec<u32>,
}

impl RegistryState {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

/// One resource whose handle is reserved but cannot be resolved until its output commits.
pub(crate) struct PendingResource {
    registry: Arc<ResourceRegistry>,
    handle: ErasedHandle,
    slot: Arc<RwLock<SlotState>>,
    value: Option<Box<dyn Any + Send + Sync>>,
    finished: bool,
}

impl PendingResource {
    pub(crate) fn commit_all(resources: &mut [Self]) -> Result<(), ResourceError> {
        let Some(registry) = resources
            .first()
            .map(|resource| Arc::clone(&resource.registry))
        else {
            return Ok(());
        };
        // Registry state guards never escape public methods, so a pending transaction cannot
        // wait on a caller-owned state guard here.
        let state = loop {
            if let Some(state) = registry.state.try_write() {
                break state;
            }
            core::hint::spin_loop();
        };
        let mut slots = Vec::with_capacity(resources.len());
        for resource in resources.iter() {
            if resource.finished || resource.value.is_none() {
                return Err(ResourceError::StaleResource);
            }
            resource.validate_registration(&registry, &state)?;
            slots.push(resource.lock_pending_slot()?);
        }
        for (resource, slot) in resources.iter_mut().zip(slots.iter_mut()) {
            slot.value = resource.value.take();
            resource.finished = true;
        }
        drop(slots);
        drop(state);
        Ok(())
    }

    pub(crate) fn rollback(mut self) {
        self.rollback_inner();
    }

    fn lock_pending_slot(&self) -> Result<RwLockWriteGuardArc<SlotState>, ResourceError> {
        // A pending slot contains no value, so public APIs cannot return an owned guard for it.
        let slot = loop {
            if let Some(slot) = self.slot.try_write_arc() {
                break slot;
            }
            core::hint::spin_loop();
        };
        validate_pending_slot(self.registry.id, self.handle, &slot)?;
        Ok(slot)
    }

    fn validate_registration(
        &self,
        registry: &Arc<ResourceRegistry>,
        state: &RegistryState,
    ) -> Result<(), ResourceError> {
        if !Arc::ptr_eq(registry, &self.registry) || self.handle.registry != registry.id {
            return Err(ResourceError::ForeignResource);
        }
        let slot = state
            .slots
            .get(self.handle.slot as usize)
            .ok_or(ResourceError::StaleResource)?;
        if !Arc::ptr_eq(slot, &self.slot) {
            return Err(ResourceError::StaleResource);
        }
        Ok(())
    }

    fn rollback_inner(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let value = self.value.take();
        let mut state = loop {
            if let Some(state) = self.registry.state.try_write() {
                break state;
            }
            core::hint::spin_loop();
        };
        if self.validate_registration(&self.registry, &state).is_err() {
            drop(state);
            drop(value);
            return;
        }
        let mut slot = loop {
            if let Some(slot) = self.slot.try_write_arc() {
                break slot;
            }
            core::hint::spin_loop();
        };
        let reusable_generation =
            if validate_pending_slot(self.registry.id, self.handle, &slot).is_ok() {
                slot.type_id = None;
                if let Some(next_generation) = slot.generation.checked_add(1) {
                    slot.generation = next_generation;
                    Some(next_generation)
                } else {
                    slot.retired = true;
                    None
                }
            } else {
                None
            };
        if let Some(generation) = reusable_generation {
            debug_assert_eq!(slot.generation, generation);
            debug_assert!(!state.free.contains(&self.handle.slot));
            state.free.push(self.handle.slot);
        }
        drop(slot);
        drop(state);
        drop(value);
    }
}

impl Drop for PendingResource {
    fn drop(&mut self) {
        self.rollback_inner();
    }
}

/// Owns host resources and resolves typed generational handles without blocking.
pub struct ResourceRegistry {
    id: RegistryId,
    state: RwLock<RegistryState>,
}

impl ResourceRegistry {
    /// Creates a registry with a process-unique identity.
    #[cfg(target_has_atomic = "ptr")]
    pub fn new() -> Result<Self, ResourceError> {
        Ok(Self::with_id(RegistryId::fresh()?))
    }

    /// Creates a registry with an identity whose uniqueness is guaranteed by the caller.
    pub const fn with_id(id: RegistryId) -> Self {
        Self {
            id,
            state: RwLock::new(RegistryState::new()),
        }
    }

    /// Returns this registry's identity.
    pub const fn id(&self) -> RegistryId {
        self.id
    }

    /// Inserts a resource and returns its typed handle.
    pub fn insert<T>(&self, value: T) -> Result<Handle<T>, ResourceError>
    where
        T: ResourceType + Send + Sync,
    {
        self.insert_slot(Some(Box::new(value)))
            .map(|(handle, _)| handle)
    }

    pub(crate) fn insert_pending<T>(
        self: &Arc<Self>,
        value: T,
    ) -> Result<(Handle<T>, PendingResource), ResourceError>
    where
        T: ResourceType + Send + Sync,
    {
        let value: Box<dyn Any + Send + Sync> = Box::new(value);
        let (handle, slot) = self.insert_slot::<T>(None)?;
        Ok((
            handle,
            PendingResource {
                registry: Arc::clone(self),
                handle: handle.erased(),
                slot,
                value: Some(value),
                finished: false,
            },
        ))
    }

    fn insert_slot<T>(
        &self,
        mut value: Option<Box<dyn Any + Send + Sync>>,
    ) -> Result<(Handle<T>, Arc<RwLock<SlotState>>), ResourceError>
    where
        T: ResourceType + Send + Sync,
    {
        let mut state = self.state.try_write().ok_or(ResourceError::RegistryBusy)?;
        let type_id = T::type_id();

        let (raw, slot) = if let Some(slot_index) = state.free.pop() {
            let Some(slot) = state.slots.get(slot_index as usize).cloned() else {
                return Err(ResourceError::StaleResource);
            };
            let Some(mut slot_state) = slot.try_write_arc() else {
                state.free.push(slot_index);
                return Err(ResourceError::ResourceBusy);
            };
            if slot_state.registry != self.id
                || slot_state.slot != slot_index
                || slot_state.retired
                || slot_state.type_id.is_some()
                || slot_state.value.is_some()
            {
                return Err(ResourceError::StaleResource);
            }
            slot_state.type_id = Some(type_id);
            slot_state.value = value.take();
            let raw = ErasedHandle {
                registry: self.id,
                slot: slot_index,
                generation: slot_state.generation,
                type_id,
            };
            drop(slot_state);
            (raw, slot)
        } else {
            let slot_index =
                u32::try_from(state.slots.len()).map_err(|_| ResourceError::CapacityExhausted)?;
            let slot = Arc::new(RwLock::new(SlotState {
                registry: self.id,
                slot: slot_index,
                generation: 0,
                type_id: Some(type_id),
                value,
                retired: false,
            }));
            let raw = ErasedHandle {
                registry: self.id,
                slot: slot_index,
                generation: 0,
                type_id,
            };
            state.slots.push(Arc::clone(&slot));
            (raw, slot)
        };

        Ok((
            Handle {
                raw,
                marker: PhantomData,
            },
            slot,
        ))
    }

    /// Tries to acquire a shared owned guard for a typed resource.
    ///
    /// This method never waits. Concurrent shared borrows succeed, while a release holding or
    /// waiting on the slot's write side produces [`ResourceError::ResourceBusy`].
    pub fn try_borrow<T>(&self, handle: Handle<T>) -> Result<ResourceReadGuard<T>, ResourceError>
    where
        T: ResourceType + Send + Sync,
    {
        let raw = handle.raw;
        if raw.registry != self.id {
            return Err(ResourceError::ForeignResource);
        }

        let state = self.state.try_read().ok_or(ResourceError::RegistryBusy)?;
        let slot = state
            .slots
            .get(raw.slot as usize)
            .cloned()
            .ok_or(ResourceError::StaleResource)?;
        drop(state);

        let guard = slot.try_read_arc().ok_or(ResourceError::ResourceBusy)?;
        validate_typed_slot::<T>(self.id, raw, &guard)?;
        Ok(ResourceReadGuard {
            guard,
            marker: PhantomData,
        })
    }

    /// Releases a live resource immediately and invalidates every copy of its handle.
    ///
    /// A resource with an active shared borrow returns [`ResourceError::ResourceBusy`]. A slot is
    /// permanently retired when advancing its generation would wrap.
    pub fn release(&self, handle: ErasedHandle) -> Result<(), ResourceError> {
        if handle.registry != self.id {
            return Err(ResourceError::ForeignResource);
        }

        let mut state = self.state.try_write().ok_or(ResourceError::RegistryBusy)?;
        let slot = state
            .slots
            .get(handle.slot as usize)
            .cloned()
            .ok_or(ResourceError::StaleResource)?;
        let mut slot_state = slot.try_write_arc().ok_or(ResourceError::ResourceBusy)?;
        validate_erased_slot(self.id, handle, &slot_state)?;

        let value = slot_state
            .value
            .take()
            .ok_or(ResourceError::StaleResource)?;
        slot_state.type_id = None;
        if let Some(next_generation) = slot_state.generation.checked_add(1) {
            slot_state.generation = next_generation;
            state.free.push(handle.slot);
        } else {
            slot_state.retired = true;
        }

        drop(slot_state);
        drop(state);
        drop(value);
        Ok(())
    }
}

fn validate_pending_slot(
    registry: RegistryId,
    handle: ErasedHandle,
    slot: &SlotState,
) -> Result<(), ResourceError> {
    if handle.registry != registry || slot.registry != registry {
        return Err(ResourceError::ForeignResource);
    }
    if handle.slot != slot.slot
        || handle.generation != slot.generation
        || slot.retired
        || slot.value.is_some()
    {
        return Err(ResourceError::StaleResource);
    }
    if handle.type_id != slot.type_id.ok_or(ResourceError::StaleResource)? {
        return Err(ResourceError::ResourceTypeMismatch);
    }
    Ok(())
}

fn validate_erased_slot(
    registry: RegistryId,
    handle: ErasedHandle,
    slot: &SlotState,
) -> Result<(), ResourceError> {
    if handle.registry != registry || slot.registry != registry {
        return Err(ResourceError::ForeignResource);
    }
    if handle.slot != slot.slot
        || handle.generation != slot.generation
        || slot.retired
        || slot.value.is_none()
    {
        return Err(ResourceError::StaleResource);
    }
    if slot.type_id != Some(handle.type_id) {
        return Err(ResourceError::ResourceTypeMismatch);
    }
    Ok(())
}

fn validate_typed_slot<T>(
    registry: RegistryId,
    handle: ErasedHandle,
    slot: &SlotState,
) -> Result<(), ResourceError>
where
    T: ResourceType + Send + Sync,
{
    validate_erased_slot(registry, handle, slot)?;
    if handle.type_id != T::type_id()
        || slot
            .value
            .as_deref()
            .and_then(|value| value.downcast_ref::<T>())
            .is_none()
    {
        return Err(ResourceError::ResourceTypeMismatch);
    }
    Ok(())
}

/// Owned shared borrow of a resource registry slot.
///
/// The guard owns the slot lock through an `Arc`; it can therefore outlive the registry borrow
/// used to resolve the handle. Dropping it permits a pending caller to try `release` again.
pub struct ResourceReadGuard<T>
where
    T: ResourceType + Send + Sync,
{
    guard: RwLockReadGuardArc<SlotState>,
    marker: PhantomData<fn() -> T>,
}

impl<T> Deref for ResourceReadGuard<T>
where
    T: ResourceType + Send + Sync,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        let Some(value) = self
            .guard
            .value
            .as_deref()
            .and_then(|value| value.downcast_ref::<T>())
        else {
            unreachable!("resource read guard changed while its shared slot lock was held");
        };
        value
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Texture(u32);

    impl ResourceType for Texture {
        const TYPE_NAME: &'static str = "Texture";
    }

    struct Window;

    impl ResourceType for Window {
        const TYPE_NAME: &'static str = "Window";
    }

    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl ResourceType for DropCounter {
        const TYPE_NAME: &'static str = "DropCounter";
    }

    fn registry(host_id: usize) -> ResourceRegistry {
        ResourceRegistry::with_id(RegistryId::from_host_id(host_id))
    }

    #[test]
    fn shared_borrows_block_release_and_release_drops_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let resources = registry(1);
        let handle = resources.insert(DropCounter(drops.clone())).unwrap();

        let first = resources.try_borrow(handle).unwrap();
        let second = resources.try_borrow(handle).unwrap();
        assert!(Arc::ptr_eq(&first.0, &drops));
        assert!(Arc::ptr_eq(&second.0, &drops));
        assert_eq!(
            resources.release(handle.erased()),
            Err(ResourceError::ResourceBusy)
        );

        drop((first, second));
        resources.release(handle.erased()).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(
            resources.try_borrow(handle).err(),
            Some(ResourceError::StaleResource)
        );
        assert_eq!(
            resources.release(handle.erased()),
            Err(ResourceError::StaleResource)
        );
    }

    #[test]
    fn reused_slot_changes_generation_and_checks_registry_and_type() {
        let resources = registry(2);
        let old = resources.insert(Texture(7)).unwrap();
        resources.release(old.erased()).unwrap();
        let current = resources.insert(Texture(9)).unwrap();

        assert_eq!(old.erased().slot(), current.erased().slot());
        assert_ne!(old.erased().generation(), current.erased().generation());
        assert_eq!(resources.try_borrow(current).unwrap().0, 9);
        assert_eq!(
            resources.try_borrow(old).err(),
            Some(ResourceError::StaleResource)
        );

        let foreign = registry(3);
        assert_eq!(
            foreign.try_borrow(current).err(),
            Some(ResourceError::ForeignResource)
        );

        let wrong_type = ErasedHandle {
            type_id: <Window as ResourceType>::type_id(),
            ..current.erased()
        };
        let wrong_type = Handle::<Window>::from_erased(wrong_type).unwrap();
        assert_eq!(
            resources.try_borrow(wrong_type).err(),
            Some(ResourceError::ResourceTypeMismatch)
        );
    }

    #[test]
    fn generation_wrap_retires_the_slot() {
        let resources = registry(4);
        let handle = resources.insert(Texture(1)).unwrap();
        let slot = {
            let state = resources.state.try_read().unwrap();
            state.slots[handle.erased().slot() as usize].clone()
        };
        slot.try_write().unwrap().generation = u32::MAX;
        let at_limit = Handle::<Texture> {
            raw: ErasedHandle {
                generation: u32::MAX,
                ..handle.erased()
            },
            marker: PhantomData,
        };

        resources.release(at_limit.erased()).unwrap();
        let replacement = resources.insert(Texture(2)).unwrap();
        assert_ne!(at_limit.erased().slot(), replacement.erased().slot());
        assert_eq!(
            resources.try_borrow(at_limit).err(),
            Some(ResourceError::StaleResource)
        );
    }

    #[cfg(target_has_atomic = "ptr")]
    #[test]
    fn fresh_registries_have_distinct_identities() {
        let first = ResourceRegistry::new().unwrap();
        let second = ResourceRegistry::new().unwrap();
        assert_ne!(first.id(), second.id());
    }
}
