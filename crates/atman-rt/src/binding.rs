//! Portable Rust value bindings used by generated host tools.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{fmt, hint::spin_loop, marker::PhantomData};

use crate::{
    HostPayload, Value,
    catalog::{ResourceSpec, TypeSpec},
    resource::{
        ErasedHandle, ResourceError, ResourcePayload, ResourceReadGuard, ResourceRegistry,
        ResourceType,
    },
    tool_router::{ToolRegisterError, ToolRouter},
};

/// Generated binding protocol used by [`ToolRouter::mount`].
///
/// `build` may construct handler and catalog entries and capture the supplied registry in those
/// handlers. It must not dispatch or execute tools, insert or release resources, or otherwise mutate
/// registry resources while the binding is being built. This keeps a failed mount free of resource
/// side effects.
pub trait Factory<P, E>: Sized {
    fn build(self, resources: Arc<ResourceRegistry>)
    -> Result<ToolRouter<P, E>, ToolRegisterError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValuePathSegment {
    Argument(String),
    Field(String),
    Index(usize),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValuePath {
    segments: Vec<ValuePathSegment>,
}

impl ValuePath {
    pub fn segments(&self) -> &[ValuePathSegment] {
        &self.segments
    }

    fn prepend(&mut self, segment: ValuePathSegment) {
        self.segments.insert(0, segment);
    }
}

impl fmt::Display for ValuePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.segments.is_empty() {
            return formatter.write_str("$");
        }
        for (index, segment) in self.segments.iter().enumerate() {
            match segment {
                ValuePathSegment::Argument(name) => {
                    if index > 0 {
                        formatter.write_str(".")?;
                    }
                    formatter.write_str(name)?;
                }
                ValuePathSegment::Field(name) => {
                    if index > 0 {
                        formatter.write_str(".")?;
                    }
                    formatter.write_str(name)?;
                }
                ValuePathSegment::Index(value) => write!(formatter, "[{value}]")?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingErrorKind {
    MissingValue,
    TypeMismatch {
        expected: Box<TypeSpec>,
        actual: String,
    },
    MissingField,
    DuplicateField,
    UnknownVariant {
        expected: Box<TypeSpec>,
        actual: String,
    },
    NumericOutOfRange {
        expected: Box<TypeSpec>,
        actual: String,
    },
    ForeignResource,
    StaleResource,
    ResourceTypeMismatch,
    ResourceBusy,
    ResourceRegistryBusy,
    ResourceContextUnavailable,
    ResourceIdentityExhausted,
    ResourceCapacityExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingError {
    path: ValuePath,
    kind: BindingErrorKind,
}

impl BindingError {
    pub fn missing_value() -> Self {
        Self::new(BindingErrorKind::MissingValue)
    }

    pub fn type_mismatch(expected: TypeSpec, actual: impl Into<String>) -> Self {
        Self::new(BindingErrorKind::TypeMismatch {
            expected: Box::new(expected),
            actual: actual.into(),
        })
    }

    pub fn missing_field(name: impl Into<String>) -> Self {
        Self::new(BindingErrorKind::MissingField).at_field(name)
    }

    pub fn duplicate_field(name: impl Into<String>) -> Self {
        Self::new(BindingErrorKind::DuplicateField).at_field(name)
    }

    pub fn unknown_variant(expected: TypeSpec, actual: impl Into<String>) -> Self {
        Self::new(BindingErrorKind::UnknownVariant {
            expected: Box::new(expected),
            actual: actual.into(),
        })
    }

    pub fn numeric_out_of_range(expected: TypeSpec, actual: impl Into<String>) -> Self {
        Self::new(BindingErrorKind::NumericOutOfRange {
            expected: Box::new(expected),
            actual: actual.into(),
        })
    }

    pub fn foreign_resource() -> Self {
        Self::new(BindingErrorKind::ForeignResource)
    }

    pub fn stale_resource() -> Self {
        Self::new(BindingErrorKind::StaleResource)
    }

    pub fn resource_type_mismatch() -> Self {
        Self::new(BindingErrorKind::ResourceTypeMismatch)
    }

    pub fn resource_busy() -> Self {
        Self::new(BindingErrorKind::ResourceBusy)
    }

    pub fn resource_registry_busy() -> Self {
        Self::new(BindingErrorKind::ResourceRegistryBusy)
    }

    pub fn resource_context_unavailable() -> Self {
        Self::new(BindingErrorKind::ResourceContextUnavailable)
    }

    pub fn resource_identity_exhausted() -> Self {
        Self::new(BindingErrorKind::ResourceIdentityExhausted)
    }

    pub fn resource_capacity_exhausted() -> Self {
        Self::new(BindingErrorKind::ResourceCapacityExhausted)
    }

    pub fn at_argument(mut self, name: impl Into<String>) -> Self {
        self.path.prepend(ValuePathSegment::Argument(name.into()));
        self
    }

    pub fn at_field(mut self, name: impl Into<String>) -> Self {
        self.path.prepend(ValuePathSegment::Field(name.into()));
        self
    }

    pub fn at_index(mut self, index: usize) -> Self {
        self.path.prepend(ValuePathSegment::Index(index));
        self
    }

    pub fn path(&self) -> &ValuePath {
        &self.path
    }

    pub fn kind(&self) -> &BindingErrorKind {
        &self.kind
    }

    const fn new(kind: BindingErrorKind) -> Self {
        Self {
            path: ValuePath {
                segments: Vec::new(),
            },
            kind,
        }
    }
}

impl fmt::Display for BindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BindingErrorKind::MissingValue => write!(formatter, "{}: missing value", self.path),
            BindingErrorKind::TypeMismatch { expected, actual } => write!(
                formatter,
                "{}: expected {expected:?}, found {actual}",
                self.path
            ),
            BindingErrorKind::MissingField => {
                write!(formatter, "{}: missing field", self.path)
            }
            BindingErrorKind::DuplicateField => {
                write!(formatter, "{}: duplicate field", self.path)
            }
            BindingErrorKind::UnknownVariant { expected, actual } => write!(
                formatter,
                "{}: expected {expected:?}, found variant {actual}",
                self.path
            ),
            BindingErrorKind::NumericOutOfRange { expected, actual } => {
                write!(formatter, "{}: {actual} is outside {expected:?}", self.path)
            }
            BindingErrorKind::ForeignResource => {
                write!(
                    formatter,
                    "{}: resource belongs to another registry",
                    self.path
                )
            }
            BindingErrorKind::StaleResource => {
                write!(formatter, "{}: resource handle is stale", self.path)
            }
            BindingErrorKind::ResourceTypeMismatch => {
                write!(formatter, "{}: resource type mismatch", self.path)
            }
            BindingErrorKind::ResourceBusy => {
                write!(formatter, "{}: resource is busy", self.path)
            }
            BindingErrorKind::ResourceRegistryBusy => {
                write!(formatter, "{}: resource registry is busy", self.path)
            }
            BindingErrorKind::ResourceContextUnavailable => {
                formatter.write_str("resource binding context is unavailable")
            }
            BindingErrorKind::ResourceIdentityExhausted => {
                formatter.write_str("resource registry identity space is exhausted")
            }
            BindingErrorKind::ResourceCapacityExhausted => {
                formatter.write_str("resource registry capacity is exhausted")
            }
        }
    }
}

impl core::error::Error for BindingError {}

impl From<ResourceError> for BindingError {
    fn from(error: ResourceError) -> Self {
        match error {
            ResourceError::IdentityExhausted => Self::resource_identity_exhausted(),
            ResourceError::CapacityExhausted => Self::resource_capacity_exhausted(),
            ResourceError::RegistryBusy => Self::resource_registry_busy(),
            ResourceError::ForeignResource => Self::foreign_resource(),
            ResourceError::StaleResource => Self::stale_resource(),
            ResourceError::ResourceTypeMismatch => Self::resource_type_mismatch(),
            ResourceError::ResourceBusy => Self::resource_busy(),
        }
    }
}

#[derive(Default)]
struct OutputTransaction {
    depth: usize,
    resources: Vec<ErasedHandle>,
}

/// Context shared by the parameters and result of one generated binding.
/// Resource-backed bindings extend this value with their registry.
pub struct Context<P, E> {
    resources: Option<Arc<ResourceRegistry>>,
    output: async_lock::Mutex<OutputTransaction>,
    marker: PhantomData<fn() -> (P, E)>,
}

impl<P, E> Context<P, E> {
    pub const fn value_only() -> Self {
        Self {
            resources: None,
            output: async_lock::Mutex::new(OutputTransaction {
                depth: 0,
                resources: Vec::new(),
            }),
            marker: PhantomData,
        }
    }

    pub fn with_resources(resources: Arc<ResourceRegistry>) -> Self {
        Self {
            resources: Some(resources),
            output: async_lock::Mutex::new(OutputTransaction::default()),
            marker: PhantomData,
        }
    }

    /// Creates an isolated context for one generated handler invocation.
    #[doc(hidden)]
    pub fn for_call(&self) -> Self {
        Self {
            resources: self.resources.clone(),
            output: async_lock::Mutex::new(OutputTransaction::default()),
            marker: PhantomData,
        }
    }

    pub fn resources(&self) -> Result<&ResourceRegistry, BindingError> {
        self.resources
            .as_deref()
            .ok_or_else(BindingError::resource_context_unavailable)
    }

    pub fn shared_resources(&self) -> Option<&Arc<ResourceRegistry>> {
        self.resources.as_ref()
    }

    /// Runs one possibly nested output encoding scope.
    ///
    /// Resource handles inserted by a failed scope are released in reverse insertion order. A
    /// successful outermost scope commits its handles to the returned value.
    #[doc(hidden)]
    pub fn output_transaction<T>(
        &self,
        encode: impl FnOnce() -> Result<T, BindingError>,
    ) -> Result<T, BindingError> {
        let checkpoint = {
            let mut output = self.lock_output();
            let checkpoint = output.resources.len();
            output.depth += 1;
            checkpoint
        };

        let result = encode();
        let rollback = {
            let mut output = self.lock_output();
            debug_assert!(output.depth > 0);
            output.depth -= 1;
            if result.is_err() {
                output.resources.split_off(checkpoint)
            } else {
                if output.depth == 0 {
                    output.resources.clear();
                }
                Vec::new()
            }
        };
        self.rollback_output_resources(rollback);
        result
    }

    /// Adds one newly inserted handle to the active output transaction.
    #[doc(hidden)]
    pub fn record_output_resource(&self, handle: ErasedHandle) {
        let mut output = self.lock_output();
        debug_assert!(output.depth > 0);
        output.resources.push(handle);
    }

    fn lock_output(&self) -> async_lock::MutexGuard<'_, OutputTransaction> {
        loop {
            if let Some(output) = self.output.try_lock() {
                return output;
            }
            spin_loop();
        }
    }

    fn rollback_output_resources(&self, mut handles: Vec<ErasedHandle>) {
        let Some(resources) = self.resources.as_deref() else {
            debug_assert!(handles.is_empty());
            return;
        };
        while let Some(handle) = handles.pop() {
            loop {
                match resources.release(handle) {
                    Ok(()) | Err(ResourceError::StaleResource) => break,
                    Err(ResourceError::RegistryBusy | ResourceError::ResourceBusy) => spin_loop(),
                    Err(error) => unreachable!("invalid output transaction handle: {error}"),
                }
            }
        }
    }
}

impl<P, E> Clone for Context<P, E> {
    fn clone(&self) -> Self {
        self.for_call()
    }
}

impl<P, E> Default for Context<P, E> {
    fn default() -> Self {
        Self::value_only()
    }
}

/// Converts one VM argument into an owned Rust value.
pub trait Input<P, E>: Sized {
    const REQUIRED: bool;

    fn input_type() -> TypeSpec;

    fn decode_input(
        value: Option<Value<P, E>>,
        context: &Context<P, E>,
    ) -> Result<Self, BindingError>;
}

/// Converts an owned Rust result into a VM value.
pub trait Output<P, E>: Sized {
    fn output_type() -> TypeSpec;

    fn encode_output(self, context: &Context<P, E>) -> Result<Value<P, E>, BindingError>;
}

/// Marks values that can be represented distinctly from `Option::None`.
#[doc(hidden)]
pub trait PresentValue {}

pub fn decode_resource_handle<P, E>(
    value: Option<Value<P, E>>,
    _context: &Context<P, E>,
) -> Result<ErasedHandle, BindingError>
where
    P: ResourcePayload,
{
    let expected = || TypeSpec::Resource(ResourceSpec { name: None });
    match value {
        Some(Value::Host(payload)) => payload
            .as_resource()
            .copied()
            .ok_or_else(|| BindingError::type_mismatch(expected(), payload.kind_name())),
        Some(other) => Err(BindingError::type_mismatch(expected(), other.kind_name())),
        None => Err(BindingError::missing_value()),
    }
}

pub fn borrow_resource<T, P, E>(
    value: Option<Value<P, E>>,
    context: &Context<P, E>,
) -> Result<ResourceReadGuard<T>, BindingError>
where
    T: ResourceType + Send + Sync,
    P: ResourcePayload,
{
    let handle = decode_resource_handle(value, context)?;
    let typed = handle.typed::<T>()?;
    context.resources()?.try_borrow(typed).map_err(Into::into)
}

macro_rules! direct_binding {
    ($type:ty, $variant:ident, $spec:expr, $kind:literal) => {
        impl<P: HostPayload, E> Input<P, E> for $type {
            const REQUIRED: bool = true;

            fn input_type() -> TypeSpec {
                $spec
            }

            fn decode_input(
                value: Option<Value<P, E>>,
                _context: &Context<P, E>,
            ) -> Result<Self, BindingError> {
                match value {
                    Some(Value::$variant(value)) => Ok(value),
                    Some(other) => Err(BindingError::type_mismatch(
                        <Self as Input<P, E>>::input_type(),
                        other.kind_name(),
                    )),
                    None => Err(BindingError::missing_value()),
                }
            }
        }

        impl<P, E> Output<P, E> for $type {
            fn output_type() -> TypeSpec {
                $spec
            }

            fn encode_output(self, _context: &Context<P, E>) -> Result<Value<P, E>, BindingError> {
                Ok(Value::$variant(self))
            }
        }

        impl PresentValue for $type {}
    };
}

direct_binding!(bool, Bool, TypeSpec::Bool, "bool");
direct_binding!(String, Str, TypeSpec::String, "string");
direct_binding!(
    i64,
    Int,
    TypeSpec::Int {
        min: i64::MIN as i128,
        max: i64::MAX as i128
    },
    "int"
);
direct_binding!(f64, Float, TypeSpec::Float { bits: 64 }, "float");

impl<P: HostPayload, E> Input<P, E> for () {
    const REQUIRED: bool = true;

    fn input_type() -> TypeSpec {
        TypeSpec::Unit
    }

    fn decode_input(
        value: Option<Value<P, E>>,
        _context: &Context<P, E>,
    ) -> Result<Self, BindingError> {
        match value {
            Some(Value::Unit) => Ok(()),
            Some(other) => Err(BindingError::type_mismatch(
                <Self as Input<P, E>>::input_type(),
                other.kind_name(),
            )),
            None => Err(BindingError::missing_value()),
        }
    }
}

impl<P, E> Output<P, E> for () {
    fn output_type() -> TypeSpec {
        TypeSpec::Unit
    }

    fn encode_output(self, _context: &Context<P, E>) -> Result<Value<P, E>, BindingError> {
        Ok(Value::Unit)
    }
}

macro_rules! checked_integer_binding {
    ($type:ty) => {
        impl<P: HostPayload, E> Input<P, E> for $type {
            const REQUIRED: bool = true;

            fn input_type() -> TypeSpec {
                TypeSpec::Int {
                    min: <$type>::MIN as i128,
                    max: <$type>::MAX as i128,
                }
            }

            fn decode_input(
                value: Option<Value<P, E>>,
                _context: &Context<P, E>,
            ) -> Result<Self, BindingError> {
                match value {
                    Some(Value::Int(value)) => <$type>::try_from(value).map_err(|_| {
                        BindingError::numeric_out_of_range(
                            <Self as Input<P, E>>::input_type(),
                            value.to_string(),
                        )
                    }),
                    Some(other) => Err(BindingError::type_mismatch(
                        <Self as Input<P, E>>::input_type(),
                        other.kind_name(),
                    )),
                    None => Err(BindingError::missing_value()),
                }
            }
        }

        impl<P, E> Output<P, E> for $type {
            fn output_type() -> TypeSpec {
                TypeSpec::Int {
                    min: <$type>::MIN as i128,
                    max: <$type>::MAX as i128,
                }
            }

            fn encode_output(self, _context: &Context<P, E>) -> Result<Value<P, E>, BindingError> {
                i64::try_from(self).map(Value::Int).map_err(|_| {
                    BindingError::numeric_out_of_range(
                        <Self as Output<P, E>>::output_type(),
                        self.to_string(),
                    )
                })
            }
        }

        impl PresentValue for $type {}
    };
}

checked_integer_binding!(i8);
checked_integer_binding!(i16);
checked_integer_binding!(i32);
checked_integer_binding!(u8);
checked_integer_binding!(u16);
checked_integer_binding!(u32);
checked_integer_binding!(u64);

impl<P: HostPayload, E> Input<P, E> for f32 {
    const REQUIRED: bool = true;

    fn input_type() -> TypeSpec {
        TypeSpec::Float { bits: 32 }
    }

    fn decode_input(
        value: Option<Value<P, E>>,
        _context: &Context<P, E>,
    ) -> Result<Self, BindingError> {
        match value {
            Some(Value::Float(value))
                if !value.is_finite() || (f32::MIN as f64..=f32::MAX as f64).contains(&value) =>
            {
                Ok(value as f32)
            }
            Some(Value::Float(value)) => Err(BindingError::numeric_out_of_range(
                <Self as Input<P, E>>::input_type(),
                value.to_string(),
            )),
            Some(other) => Err(BindingError::type_mismatch(
                <Self as Input<P, E>>::input_type(),
                other.kind_name(),
            )),
            None => Err(BindingError::missing_value()),
        }
    }
}

impl<P, E> Output<P, E> for f32 {
    fn output_type() -> TypeSpec {
        TypeSpec::Float { bits: 32 }
    }

    fn encode_output(self, _context: &Context<P, E>) -> Result<Value<P, E>, BindingError> {
        Ok(Value::Float(self as f64))
    }
}

impl PresentValue for f32 {}

impl<P: HostPayload, E, T: Input<P, E>> Input<P, E> for Vec<T> {
    const REQUIRED: bool = true;

    fn input_type() -> TypeSpec {
        TypeSpec::List(Box::new(T::input_type()))
    }

    fn decode_input(
        value: Option<Value<P, E>>,
        context: &Context<P, E>,
    ) -> Result<Self, BindingError> {
        let Some(value) = value else {
            return Err(BindingError::missing_value());
        };
        let Value::List(items) = value else {
            return Err(BindingError::type_mismatch(
                <Self as Input<P, E>>::input_type(),
                value.kind_name(),
            ));
        };
        items
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                T::decode_input(Some(value), context).map_err(|error| error.at_index(index))
            })
            .collect()
    }
}

impl<P, E, T: Output<P, E>> Output<P, E> for Vec<T> {
    fn output_type() -> TypeSpec {
        TypeSpec::List(Box::new(T::output_type()))
    }

    fn encode_output(self, context: &Context<P, E>) -> Result<Value<P, E>, BindingError> {
        context.output_transaction(|| {
            self.into_iter()
                .enumerate()
                .map(|(index, value)| {
                    value
                        .encode_output(context)
                        .map_err(|error| error.at_index(index))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::List)
        })
    }
}

impl<T> PresentValue for Vec<T> {}

impl<P: HostPayload, E, T> Input<P, E> for Option<T>
where
    T: Input<P, E> + PresentValue,
{
    const REQUIRED: bool = false;

    fn input_type() -> TypeSpec {
        TypeSpec::Option(Box::new(T::input_type()))
    }

    fn decode_input(
        value: Option<Value<P, E>>,
        context: &Context<P, E>,
    ) -> Result<Self, BindingError> {
        match value {
            None | Some(Value::Unit) => Ok(None),
            Some(value) => T::decode_input(Some(value), context).map(Some),
        }
    }
}

impl<P, E, T> Output<P, E> for Option<T>
where
    T: Output<P, E> + PresentValue,
{
    fn output_type() -> TypeSpec {
        TypeSpec::Option(Box::new(T::output_type()))
    }

    fn encode_output(self, context: &Context<P, E>) -> Result<Value<P, E>, BindingError> {
        context.output_transaction(|| match self {
            Some(value) => value.encode_output(context),
            None => Ok(Value::Unit),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    type TestValue = Value<(), ()>;

    #[test]
    fn nested_decode_reports_the_full_path() {
        let error = Vec::<i8>::decode_input(
            Some(TestValue::List(vec![
                TestValue::Int(1),
                TestValue::Int(128),
            ])),
            &Context::value_only(),
        )
        .unwrap_err()
        .at_argument("values");

        assert_eq!(error.path().to_string(), "values[1]");
        assert!(matches!(
            error.kind(),
            BindingErrorKind::NumericOutOfRange { .. }
        ));
    }

    #[test]
    fn optional_input_distinguishes_missing_from_required() {
        assert_eq!(
            Option::<String>::decode_input(None, &Context::<(), ()>::value_only()).unwrap(),
            None
        );
        assert!(matches!(
            String::decode_input(None, &Context::<(), ()>::value_only())
                .unwrap_err()
                .kind(),
            BindingErrorKind::MissingValue
        ));
    }

    #[test]
    fn integer_conversion_checks_both_directions() {
        assert!(
            u8::decode_input(Some(TestValue::Int(-1)), &Context::<(), ()>::value_only()).is_err()
        );
        assert!(
            u64::MAX
                .encode_output(&Context::<(), ()>::value_only())
                .is_err()
        );
    }
}
