//! Closure-based tool bindings for small VM embeddings.

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{fmt, future::Future};

use crate::{
    EffectDelegate, ExpressionEffect, HostFuture, HostPayload, HostValueOps, ListIntrinsic,
    ToolCallMode, Value, ValueError, VmContext,
    binding::Factory,
    catalog::{CatalogEntry, ToolCatalog, ToolSpec},
    resource::{ResourceError, ResourceRegistry},
};

/// Evaluated arguments passed to a host tool. Named arguments take precedence
/// over the corresponding positional argument in the typed accessors.
#[derive(Debug)]
pub struct ToolArgs<P, E> {
    pub positional: Vec<Value<P, E>>,
    pub named: Vec<(String, Value<P, E>)>,
}

impl<P, E> ToolArgs<P, E> {
    pub fn positional(&self, index: usize) -> Option<&Value<P, E>> {
        self.positional.get(index)
    }

    pub fn named(&self, name: &str) -> Option<&Value<P, E>> {
        self.named
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// Consumes one argument while preserving named-over-positional precedence.
    pub fn take(&mut self, name: &str, position: usize) -> Option<Value<P, E>> {
        if let Some(index) = self.named.iter().position(|(key, _)| key == name) {
            return Some(self.named.remove(index).1);
        }
        self.positional
            .get_mut(position)
            .map(|value| core::mem::replace(value, Value::Unit))
    }
}

impl<P: HostPayload, E: ValueError> ToolArgs<P, E> {
    pub fn get(&self, name: &str, position: usize) -> Result<&Value<P, E>, E> {
        self.named(name)
            .or_else(|| self.positional(position))
            .ok_or_else(|| E::missing_argument(name))
    }

    pub fn int(&self, name: &str, position: usize) -> Result<i64, E> {
        let value = self.get(name, position)?;
        match value {
            Value::Int(value) => Ok(*value),
            _ => Err(E::type_mismatch("int", value.kind_name().into())),
        }
    }

    pub fn float(&self, name: &str, position: usize) -> Result<f64, E> {
        let value = self.get(name, position)?;
        match value {
            Value::Float(value) => Ok(*value),
            _ => Err(E::type_mismatch("float", value.kind_name().into())),
        }
    }

    pub fn bool(&self, name: &str, position: usize) -> Result<bool, E> {
        let value = self.get(name, position)?;
        match value {
            Value::Bool(value) => Ok(*value),
            _ => Err(E::type_mismatch("bool", value.kind_name().into())),
        }
    }

    pub fn string(&self, name: &str, position: usize) -> Result<&str, E> {
        let value = self.get(name, position)?;
        match value {
            Value::Str(value) => Ok(value),
            _ => Err(E::type_mismatch("string", value.kind_name().into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRegisterError {
    EmptyName,
    ReservedName(String),
    DuplicateName(String),
    MountInProgress,
    ModeMismatch {
        name: String,
        expected: ToolCallMode,
        actual: ToolCallMode,
    },
    Resource(ResourceError),
}

impl fmt::Display for ToolRegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => f.write_str("tool name cannot be empty"),
            Self::ReservedName(name) => write!(f, "tool name `{name}` is reserved"),
            Self::DuplicateName(name) => write!(f, "tool name `{name}` is already registered"),
            Self::MountInProgress => {
                f.write_str("another binding mount is already in progress for this router lineage")
            }
            Self::ModeMismatch {
                name,
                expected,
                actual,
            } => write!(
                f,
                "tool `{name}` mode mismatch: expected {expected:?}, found {actual:?}"
            ),
            Self::Resource(error) => write!(f, "failed to create tool resources: {error}"),
        }
    }
}

impl core::error::Error for ToolRegisterError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Resource(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ResourceError> for ToolRegisterError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

type ToolHandler<P, E> =
    dyn Fn(ToolArgs<P, E>) -> HostFuture<'static, Result<Value<P, E>, E>> + Send + Sync;

struct RegisteredTool<P, E> {
    mode: ToolCallMode,
    spec: Option<ToolSpec>,
    handler: Arc<ToolHandler<P, E>>,
}

/// A build-time registry of immediate and deferred host tool handlers.
/// Clones share a resource lineage and one handler snapshot. Registering on either clone creates a
/// new handler snapshot.
pub struct ToolRouter<P, E> {
    handlers: Arc<BTreeMap<String, Arc<RegisteredTool<P, E>>>>,
    resource_lineage: Arc<async_lock::Mutex<Weak<ResourceRegistry>>>,
    mounted_resources: Option<Arc<ResourceRegistry>>,
}

impl<P, E> Clone for ToolRouter<P, E> {
    fn clone(&self) -> Self {
        Self {
            handlers: Arc::clone(&self.handlers),
            resource_lineage: Arc::clone(&self.resource_lineage),
            mounted_resources: self.mounted_resources.clone(),
        }
    }
}

impl<P, E> Default for ToolRouter<P, E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P, E> ToolRouter<P, E> {
    pub fn new() -> Self {
        Self {
            handlers: Arc::new(BTreeMap::new()),
            resource_lineage: Arc::new(async_lock::Mutex::new(Weak::new())),
            mounted_resources: None,
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    /// Returns a stable-order projection of every registered tool.
    pub fn catalog(&self) -> ToolCatalog {
        ToolCatalog::new(
            self.handlers
                .iter()
                .map(|(name, tool)| CatalogEntry {
                    name: name.clone(),
                    mode: tool.mode,
                    spec: tool.spec.clone(),
                })
                .collect(),
        )
    }

    /// Atomically installs all handler and catalog entries built by one generated binding factory.
    /// The factory's shared resource registry joins this router lineage only after installation
    /// succeeds.
    pub fn mount<F>(&mut self, factory: F) -> Result<(), ToolRegisterError>
    where
        F: Factory<P, E>,
    {
        if let Some(resources) = &self.mounted_resources {
            let mounted = factory.build(Arc::clone(resources))?;
            return self.commit_mount(mounted);
        }

        let lineage = Arc::clone(&self.resource_lineage);
        let mut registered = lineage
            .try_lock()
            .ok_or(ToolRegisterError::MountInProgress)?;
        let resources = if let Some(resources) = registered.upgrade() {
            resources
        } else {
            Arc::new(ResourceRegistry::new()?)
        };
        let mounted = factory.build(Arc::clone(&resources))?;
        self.commit_mount(mounted)?;
        *registered = Arc::downgrade(&resources);
        self.mounted_resources = Some(resources);
        Ok(())
    }

    fn commit_mount(&mut self, mounted: Self) -> Result<(), ToolRegisterError> {
        if let Some(name) = mounted
            .handlers
            .keys()
            .find(|name| self.handlers.contains_key(name.as_str()))
        {
            return Err(ToolRegisterError::DuplicateName(name.clone()));
        }

        Arc::make_mut(&mut self.handlers).extend(
            mounted
                .handlers
                .iter()
                .map(|(name, tool)| (name.clone(), Arc::clone(tool))),
        );
        Ok(())
    }

    /// Registers a cold asynchronous tool. Its handler runs on `.await` or `fanout`.
    pub fn register<F, Fut>(
        &mut self,
        name: impl Into<String>,
        handler: F,
    ) -> Result<(), ToolRegisterError>
    where
        P: 'static,
        E: 'static,
        F: Fn(ToolArgs<P, E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value<P, E>, E>> + Send + 'static,
    {
        self.insert(name.into(), ToolCallMode::Deferred, None, move |args| {
            Box::pin(handler(args))
        })
    }

    /// Registers a cold asynchronous tool with its portable signature.
    pub fn register_with_spec<F, Fut>(
        &mut self,
        spec: ToolSpec,
        handler: F,
    ) -> Result<(), ToolRegisterError>
    where
        P: 'static,
        E: 'static,
        F: Fn(ToolArgs<P, E>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value<P, E>, E>> + Send + 'static,
    {
        self.insert_spec(spec, ToolCallMode::Deferred, move |args| {
            Box::pin(handler(args))
        })
    }

    /// Registers a synchronous tool that runs when its call expression is evaluated.
    pub fn register_sync<F>(
        &mut self,
        name: impl Into<String>,
        handler: F,
    ) -> Result<(), ToolRegisterError>
    where
        P: Send + Sync + 'static,
        E: Send + Sync + 'static,
        F: Fn(ToolArgs<P, E>) -> Result<Value<P, E>, E> + Send + Sync + 'static,
    {
        self.insert(name.into(), ToolCallMode::Immediate, None, move |args| {
            let result = handler(args);
            Box::pin(async move { result })
        })
    }

    /// Registers a synchronous tool with its portable signature.
    pub fn register_sync_with_spec<F>(
        &mut self,
        spec: ToolSpec,
        handler: F,
    ) -> Result<(), ToolRegisterError>
    where
        P: Send + Sync + 'static,
        E: Send + Sync + 'static,
        F: Fn(ToolArgs<P, E>) -> Result<Value<P, E>, E> + Send + Sync + 'static,
    {
        self.insert_spec(spec, ToolCallMode::Immediate, move |args| {
            let result = handler(args);
            Box::pin(async move { result })
        })
    }

    fn insert_spec(
        &mut self,
        spec: ToolSpec,
        mode: ToolCallMode,
        handler: impl Fn(ToolArgs<P, E>) -> HostFuture<'static, Result<Value<P, E>, E>>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), ToolRegisterError> {
        if spec.mode != mode {
            return Err(ToolRegisterError::ModeMismatch {
                name: spec.name,
                expected: mode,
                actual: spec.mode,
            });
        }
        let name = spec.name.clone();
        self.insert(name, mode, Some(spec), handler)
    }

    fn insert(
        &mut self,
        name: String,
        mode: ToolCallMode,
        spec: Option<ToolSpec>,
        handler: impl Fn(ToolArgs<P, E>) -> HostFuture<'static, Result<Value<P, E>, E>>
        + Send
        + Sync
        + 'static,
    ) -> Result<(), ToolRegisterError> {
        if name.trim().is_empty() {
            return Err(ToolRegisterError::EmptyName);
        }
        if name == "env" || ListIntrinsic::from_name(&name).is_some() {
            return Err(ToolRegisterError::ReservedName(name));
        }
        if self.contains(&name) {
            return Err(ToolRegisterError::DuplicateName(name));
        }
        Arc::make_mut(&mut self.handlers).insert(
            name,
            Arc::new(RegisteredTool {
                mode,
                spec,
                handler: Arc::new(handler),
            }),
        );
        Ok(())
    }
}

impl<P: HostValueOps, E: ValueError> ToolRouter<P, E> {
    /// Looks up the handler again at dispatch time. Handler errors become `Value::Err`.
    pub async fn dispatch(&self, name: &str, args: ToolArgs<P, E>) -> Value<P, E> {
        let Some(handler) = self.handlers.get(name) else {
            return Value::Err(E::type_mismatch("registered tool", name.to_string()));
        };
        match (handler.handler)(args).await {
            Ok(value) => value,
            Err(error) => Value::Err(error),
        }
    }
}

impl<P, E> EffectDelegate for ToolRouter<P, E>
where
    P: HostValueOps + Clone + Send + Sync + 'static,
    E: ValueError + Clone + Send + Sync + 'static,
{
    type Payload = P;
    type Error = E;
    type Permit = ();

    fn preflight_tool(&self, name: &str, _context: &VmContext) -> Option<Value<P, E>> {
        (!self.contains(name))
            .then(|| Value::Err(E::type_mismatch("registered tool", name.to_string())))
    }

    fn tool_call_mode(&self, name: &str, _context: &VmContext) -> ToolCallMode {
        self.handlers
            .get(name)
            .map_or(ToolCallMode::Immediate, |tool| tool.mode)
    }

    fn invoke<'a>(
        &'a self,
        effect: ExpressionEffect<P, E>,
        _permit: Self::Permit,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Value<P, E>> {
        Box::pin(async move {
            match effect {
                ExpressionEffect::ToolCall {
                    name,
                    positional,
                    named,
                    ..
                } => self.dispatch(&name, ToolArgs { positional, named }).await,
                ExpressionEffect::FileRef(_) => unsupported_effect("file reference"),
                ExpressionEffect::Confirm(_) => unsupported_effect("confirmation"),
                ExpressionEffect::Message { .. } => unsupported_effect("message"),
                ExpressionEffect::Call { .. } => unsupported_effect("external call"),
                ExpressionEffect::FixSnapshot { .. } => unsupported_effect("fix snapshot"),
                ExpressionEffect::FixRestore { .. } => unsupported_effect("fix restore"),
            }
        })
    }
}

fn unsupported_effect<P, E: ValueError>(name: &str) -> Value<P, E> {
    Value::Err(E::type_mismatch("registered tool call", name.to_string()))
}
