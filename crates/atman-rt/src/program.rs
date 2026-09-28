//! Pure module linking and flow-reference validation.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use crate::ast::{
    Arg, Expr, File, FlowDecl, FlowRef, LifecycleEvent, Node, Stmt, UseBinding, WatchAction,
};

pub const MAX_MODULES: usize = 128;
pub const MAX_DEPTH: usize = 32;
pub const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

/// A source unit identified by a stable, host-defined logical ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub text: String,
}

impl Source {
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
        }
    }
}

/// The host supplies source text and enforces its own path and trust policy.
pub trait SourceResolver {
    type Error: fmt::Display;

    fn resolve(&self, importer_id: &str, specifier: &str) -> Result<Source, Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    Source {
        importer: String,
        specifier: String,
        detail: String,
    },
    Parse {
        source_id: String,
        detail: String,
    },
    Graph(String),
    Link(LinkError),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source {
                importer,
                specifier,
                detail,
            } => write!(f, "{importer}: use {specifier:?}: {detail}"),
            Self::Parse { source_id, detail } => write!(f, "parse source {source_id}: {detail}"),
            Self::Graph(detail) => f.write_str(detail),
            Self::Link(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for CompileError {}

impl From<LinkError> for CompileError {
    fn from(error: LinkError) -> Self {
        Self::Link(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowId {
    pub module: ModuleId,
    pub name: String,
}

impl FlowId {
    /// A key disjoint from DSL identifiers for string-keyed host registries.
    pub fn runtime_key(&self) -> String {
        format!("@{}:{}", self.module.0, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError {
    message: String,
}

impl LinkError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(self, context: impl AsRef<str>) -> Self {
        Self::new(format!("{}: {}", context.as_ref(), self.message))
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl core::error::Error for LinkError {}

/// Source text and parsed syntax supplied by a host. The host resolves each `use` to a module ID.
#[derive(Debug, Clone)]
pub struct ModuleInput {
    pub source_id: String,
    pub display_name: String,
    pub source: String,
    pub file: File,
    pub dependencies: BTreeMap<String, ModuleId>,
}

#[derive(Debug, Clone)]
struct LinkedModule {
    input: ModuleInput,
    names: BTreeMap<String, FlowId>,
    namespaces: BTreeMap<String, ModuleId>,
    public: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct LinkedProgram {
    modules: Vec<LinkedModule>,
    entry: ModuleId,
}

impl LinkedProgram {
    /// Parse an entry source, load its imports through the host, and link the full program.
    #[cfg(feature = "syntax")]
    pub fn compile<R: SourceResolver>(entry: Source, resolver: &R) -> Result<Self, CompileError> {
        let mut loader = Compiler {
            resolver,
            inputs: Vec::new(),
            by_id: BTreeMap::new(),
            stack: Vec::new(),
            source_bytes: 0,
        };
        let entry_id = loader.load(entry, true)?;
        Self::link(loader.inputs, entry_id).map_err(CompileError::from)
    }

    /// Link already-loaded source units without filesystem or product-specific I/O.
    pub fn link(inputs: Vec<ModuleInput>, entry: ModuleId) -> Result<Self, LinkError> {
        if inputs.is_empty() || inputs.len() > MAX_MODULES || entry.0 >= inputs.len() {
            return Err(LinkError::new("invalid module graph"));
        }
        let mut ids = BTreeSet::new();
        let mut source_bytes = 0usize;
        let mut modules = Vec::with_capacity(inputs.len());
        for (index, input) in inputs.into_iter().enumerate() {
            if input.source_id.is_empty() {
                return Err(LinkError::new("source ID must not be empty"));
            }
            if !ids.insert(input.source_id.clone()) {
                return Err(LinkError::new(format!(
                    "duplicate source ID `{}`",
                    input.source_id
                )));
            }
            source_bytes = source_bytes
                .checked_add(input.source.len())
                .filter(|bytes| *bytes <= MAX_SOURCE_BYTES)
                .ok_or_else(|| {
                    LinkError::new(format!("source graph exceeds {MAX_SOURCE_BYTES} bytes"))
                })?;
            if index != entry.0
                && (!input.file.routes.is_empty()
                    || input.file.default_route.is_some()
                    || !input.file.lifecycles.is_empty())
            {
                return Err(LinkError::new(format!(
                    "dependency source {} contains `route`, `default_route`, or `on`; only entry files may declare them",
                    input.display_name
                )));
            }
            let mut names = BTreeMap::new();
            for flow in &input.file.flows {
                let name = flow.name.name.clone();
                if names
                    .insert(
                        name.clone(),
                        FlowId {
                            module: ModuleId(index),
                            name: name.clone(),
                        },
                    )
                    .is_some()
                {
                    return Err(LinkError::new(format!(
                        "{}: duplicate flow `{name}`",
                        input.display_name
                    )));
                }
            }
            let mut public = BTreeSet::new();
            for name in &input.file.public_flows {
                if !names.contains_key(&name.name) {
                    return Err(LinkError::new(format!(
                        "{}: `pub flow {}` is not declared",
                        input.display_name, name.name
                    )));
                }
                if !public.insert(name.name.clone()) {
                    return Err(LinkError::new(format!(
                        "{}: duplicate `pub flow {}`",
                        input.display_name, name.name
                    )));
                }
            }
            modules.push(LinkedModule {
                input,
                names,
                namespaces: BTreeMap::new(),
                public,
            });
        }
        let mut program = Self { modules, entry };
        program.check_dependencies()?;
        program.bind_uses()?;
        program.validate_calls()?;
        Ok(program)
    }

    pub fn entry_module(&self) -> ModuleId {
        self.entry
    }

    pub fn entry_file(&self) -> &File {
        &self.modules[self.entry.0].input.file
    }

    pub fn entry_source(&self) -> &str {
        &self.modules[self.entry.0].input.source
    }

    pub fn route(&self, input: &str) -> Option<crate::route::RouteMatch> {
        crate::route::resolve_route(self.entry_file(), input)
    }

    pub fn lifecycle_flows(&self, event: LifecycleEvent) -> impl Iterator<Item = FlowDecl> + '_ {
        crate::lifecycle::lifecycle_hooks(self.entry_file(), event)
            .map(|(index, hook)| crate::lifecycle::lifecycle_flow(hook, index))
    }

    pub fn module(&self, id: ModuleId) -> Option<&ModuleInput> {
        self.modules.get(id.0).map(|module| &module.input)
    }

    pub fn modules(&self) -> impl Iterator<Item = (ModuleId, &ModuleInput)> {
        self.modules
            .iter()
            .enumerate()
            .map(|(index, module)| (ModuleId(index), &module.input))
    }

    /// Entry selection never sees imported names.
    pub fn entry_flow(&self, name: &str) -> Option<FlowId> {
        self.entry_file()
            .flows
            .iter()
            .find(|flow| flow.name.name == name)
            .map(|_| FlowId {
                module: self.entry,
                name: name.to_string(),
            })
    }

    pub fn resolve(&self, caller: ModuleId, target: &FlowRef) -> Option<FlowId> {
        let module = self.modules.get(caller.0)?;
        match target {
            FlowRef::Local(name) => module.names.get(&name.name).cloned(),
            FlowRef::Qualified {
                module: alias,
                flow,
            } => {
                let target_module = *module.namespaces.get(&alias.name)?;
                self.modules[target_module.0]
                    .public
                    .contains(&flow.name)
                    .then(|| FlowId {
                        module: target_module,
                        name: flow.name.clone(),
                    })
            }
        }
    }

    pub fn flow(&self, id: &FlowId) -> Option<&FlowDecl> {
        self.modules
            .get(id.module.0)?
            .input
            .file
            .flows
            .iter()
            .find(|flow| flow.name.name == id.name)
    }

    pub fn iter_flows(&self) -> impl Iterator<Item = (FlowId, &FlowDecl)> {
        self.modules.iter().enumerate().flat_map(|(index, module)| {
            module.input.file.flows.iter().map(move |flow| {
                (
                    FlowId {
                        module: ModuleId(index),
                        name: flow.name.name.clone(),
                    },
                    flow,
                )
            })
        })
    }

    /// Attach a synthetic entry flow without changing the source closure.
    pub fn with_entry_flow(&self, flow: FlowDecl) -> Result<Self, LinkError> {
        let mut program = self.clone();
        let module = &mut program.modules[program.entry.0];
        let name = flow.name.name.clone();
        ensure_unbound(module, &name)?;
        module.names.insert(
            name.clone(),
            FlowId {
                module: program.entry,
                name,
            },
        );
        module.input.file.flows.push(flow);
        program.validate_calls()?;
        Ok(program)
    }

    fn check_dependencies(&self) -> Result<(), LinkError> {
        for module in &self.modules {
            for use_decl in &module.input.file.uses {
                let Some(target) = module.input.dependencies.get(&use_decl.source) else {
                    return Err(LinkError::new(format!(
                        "{}: use {:?} has no resolved source",
                        module.input.display_name, use_decl.source
                    )));
                };
                if target.0 >= self.modules.len() {
                    return Err(LinkError::new(format!(
                        "{}: use {:?} has invalid module ID {}",
                        module.input.display_name, use_decl.source, target.0
                    )));
                }
            }
            if module.input.dependencies.keys().any(|source| {
                !module
                    .input
                    .file
                    .uses
                    .iter()
                    .any(|use_decl| &use_decl.source == source)
            }) {
                return Err(LinkError::new(format!(
                    "{}: undeclared source dependency",
                    module.input.display_name
                )));
            }
        }
        let mut depths = BTreeMap::new();
        for index in 0..self.modules.len() {
            self.dependency_depth(ModuleId(index), &mut Vec::new(), &mut depths)?;
        }
        let mut reachable = BTreeSet::new();
        let mut pending = Vec::from([self.entry]);
        while let Some(id) = pending.pop() {
            if reachable.insert(id) {
                pending.extend(self.modules[id.0].input.dependencies.values().copied());
            }
        }
        if reachable.len() != self.modules.len() {
            return Err(LinkError::new(
                "module graph contains unreachable source files",
            ));
        }
        Ok(())
    }

    fn dependency_depth(
        &self,
        id: ModuleId,
        stack: &mut Vec<ModuleId>,
        depths: &mut BTreeMap<ModuleId, usize>,
    ) -> Result<usize, LinkError> {
        if stack.contains(&id) {
            let chain = stack
                .iter()
                .copied()
                .chain(core::iter::once(id))
                .map(|member| self.modules[member.0].input.display_name.as_str())
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(LinkError::new(format!("source dependency cycle: {chain}")));
        }
        if let Some(depth) = depths.get(&id) {
            return Ok(*depth);
        }
        stack.push(id);
        let mut depth = 1usize;
        for target in self.modules[id.0].input.dependencies.values() {
            depth = depth.max(1 + self.dependency_depth(*target, stack, depths)?);
        }
        stack.pop();
        if depth > MAX_DEPTH {
            return Err(LinkError::new(format!(
                "source dependency depth exceeds {MAX_DEPTH}: {}",
                self.modules[id.0].input.display_name
            )));
        }
        depths.insert(id, depth);
        Ok(depth)
    }

    fn bind_uses(&mut self) -> Result<(), LinkError> {
        for caller in 0..self.modules.len() {
            let uses = self.modules[caller].input.file.uses.clone();
            for use_decl in uses {
                let dependency = self.modules[caller].input.dependencies[&use_decl.source];
                match use_decl.binding {
                    UseBinding::Module(alias) => {
                        let module = &mut self.modules[caller];
                        ensure_unbound(module, &alias.name)?;
                        module.namespaces.insert(alias.name, dependency);
                    }
                    UseBinding::Flows(bindings) => {
                        for binding in bindings {
                            let dependency_module = &self.modules[dependency.0];
                            if !dependency_module.public.contains(&binding.name.name) {
                                let kind = if dependency_module
                                    .input
                                    .file
                                    .flows
                                    .iter()
                                    .any(|flow| flow.name.name == binding.name.name)
                                {
                                    format!(
                                        "flow `{}` in {} is private; declare it as `pub flow`",
                                        binding.name.name, dependency_module.input.display_name
                                    )
                                } else {
                                    format!(
                                        "flow `{}` does not exist in {}",
                                        binding.name.name, dependency_module.input.display_name
                                    )
                                };
                                return Err(LinkError::new(format!(
                                    "{}: use {:?}: {kind}",
                                    self.modules[caller].input.display_name, use_decl.source
                                )));
                            }
                            let local =
                                binding.alias.as_ref().unwrap_or(&binding.name).name.clone();
                            let module = &mut self.modules[caller];
                            ensure_unbound(module, &local)?;
                            module.names.insert(
                                local,
                                FlowId {
                                    module: dependency,
                                    name: binding.name.name,
                                },
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn resolve_checked(&self, caller: ModuleId, target: &FlowRef) -> Result<FlowId, LinkError> {
        if let Some(id) = self.resolve(caller, target) {
            return Ok(id);
        }
        let module = &self.modules[caller.0];
        match target {
            FlowRef::Local(name) if module.namespaces.contains_key(&name.name) => {
                Err(LinkError::new(format!(
                    "`{}` is a module alias; use `{}.flow`",
                    name.name, name.name
                )))
            }
            FlowRef::Local(name) => Err(LinkError::new(format!("undefined flow `{}`", name.name))),
            FlowRef::Qualified {
                module: alias,
                flow,
            } => {
                let Some(target_id) = module.namespaces.get(&alias.name) else {
                    return Err(LinkError::new(format!(
                        "undefined module alias `{}`",
                        alias.name
                    )));
                };
                let target_module = &self.modules[target_id.0];
                let kind = if target_module
                    .input
                    .file
                    .flows
                    .iter()
                    .any(|decl| decl.name.name == flow.name)
                {
                    format!(
                        "flow `{}` in {} is private; declare it as `pub flow`",
                        flow.name, target_module.input.display_name
                    )
                } else {
                    format!(
                        "flow `{}` does not exist in {}",
                        flow.name, target_module.input.display_name
                    )
                };
                Err(LinkError::new(kind))
            }
        }
    }

    fn validate_calls(&self) -> Result<(), LinkError> {
        for (index, module) in self.modules.iter().enumerate() {
            let id = ModuleId(index);
            for flow in &module.input.file.flows {
                for param in &flow.params {
                    if let Some(default) = &param.default {
                        self.check_expr(id, default).map_err(|error| {
                            error.context(format!(
                                "{}: flow `{}` parameter default",
                                module.input.display_name, flow.name.name
                            ))
                        })?;
                    }
                }
                if let Some(contract) = &flow.contract {
                    for block in &contract.blocks {
                        for (_, expr) in &block.kwargs {
                            self.check_expr(id, expr).map_err(|error| {
                                error.context(format!(
                                    "{}: flow `{}` contract",
                                    module.input.display_name, flow.name.name
                                ))
                            })?;
                        }
                    }
                }
                self.check_stmts(id, &flow.body).map_err(|error| {
                    error.context(format!(
                        "{}: flow `{}`",
                        module.input.display_name, flow.name.name
                    ))
                })?;
            }
            for lifecycle in &module.input.file.lifecycles {
                self.check_stmts(id, &lifecycle.body).map_err(|error| {
                    error.context(format!(
                        "{}: lifecycle {:?}",
                        module.input.display_name, lifecycle.event
                    ))
                })?;
            }
        }
        Ok(())
    }

    fn check_stmts(&self, caller: ModuleId, stmts: &[Stmt]) -> Result<(), LinkError> {
        for stmt in stmts {
            match stmt {
                Stmt::Bind { value, .. } | Stmt::Return { value } | Stmt::Expr(value) => {
                    self.check_expr(caller, value)?;
                }
                Stmt::When { cond, body } => {
                    self.check_expr(caller, cond)?;
                    self.check_stmts(caller, body)?;
                }
                Stmt::Loop { body } => self.check_stmts(caller, body)?,
                Stmt::Watch(watch) => {
                    for block in &watch.on_blocks {
                        for action in &block.actions {
                            match action {
                                WatchAction::Abort { msg: Some(expr) }
                                | WatchAction::Warn { msg: Some(expr) } => {
                                    self.check_expr(caller, expr)?;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Stmt::Break | Stmt::Continue => {}
            }
        }
        Ok(())
    }

    fn check_args(&self, caller: ModuleId, args: &[Arg]) -> Result<(), LinkError> {
        for arg in args {
            match arg {
                Arg::Positional(expr) | Arg::Named { value: expr, .. } => {
                    self.check_expr(caller, expr)?;
                }
            }
        }
        Ok(())
    }

    fn check_expr(&self, caller: ModuleId, expr: &Expr) -> Result<(), LinkError> {
        match expr {
            Expr::Literal(_) | Expr::Ident(_) | Expr::FileRef(_) => {}
            Expr::Member { base, .. } | Expr::Unary { operand: base, .. } => {
                self.check_expr(caller, base)?;
            }
            Expr::Binary { left, right, .. } => {
                self.check_expr(caller, left)?;
                self.check_expr(caller, right)?;
            }
            Expr::Call { args, .. } | Expr::List(args) => {
                for arg in args {
                    self.check_expr(caller, arg)?;
                }
            }
            Expr::Struct(fields) => {
                for (_, value) in fields {
                    self.check_expr(caller, value)?;
                }
            }
            Expr::Annotated { expr, .. } => self.check_expr(caller, expr)?,
            Expr::Lambda { body, .. } => self.check_expr(caller, body)?,
            Expr::Node(node) => match node {
                Node::Subflow { name, args } => {
                    self.resolve_checked(caller, name).map_err(|error| {
                        error.context(format!("subflow({})", name.display_name()))
                    })?;
                    self.check_args(caller, args)?;
                }
                Node::ToolCall { args, .. } | Node::Message { args, .. } => {
                    self.check_args(caller, args)?;
                }
                Node::Fanout { source } => self.check_expr(caller, source)?,
                Node::DynamicFanout { source, lambda } => {
                    self.check_expr(caller, source)?;
                    self.check_expr(caller, lambda)?;
                }
                Node::UserConfirm { msg } => self.check_expr(caller, msg)?,
                Node::FixUntilTestPasses { kwargs } => {
                    for (_, value) in kwargs {
                        self.check_expr(caller, value)?;
                    }
                }
            },
        }
        Ok(())
    }
}

#[cfg(feature = "syntax")]
struct Compiler<'a, R> {
    resolver: &'a R,
    inputs: Vec<ModuleInput>,
    by_id: BTreeMap<String, ModuleId>,
    stack: Vec<String>,
    source_bytes: usize,
}

#[cfg(feature = "syntax")]
impl<R: SourceResolver> Compiler<'_, R> {
    fn load(&mut self, source: Source, entry: bool) -> Result<ModuleId, CompileError> {
        if self.stack.contains(&source.id) {
            let chain = self
                .stack
                .iter()
                .map(String::as_str)
                .chain(core::iter::once(source.id.as_str()))
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(CompileError::Graph(format!(
                "source dependency cycle: {chain}"
            )));
        }
        if let Some(id) = self.by_id.get(&source.id) {
            if self.inputs[id.0].source != source.text {
                return Err(CompileError::Graph(format!(
                    "source ID `{}` resolved to different text",
                    source.id
                )));
            }
            return Ok(*id);
        }
        if source.id.is_empty() {
            return Err(CompileError::Graph(
                "source ID must not be empty".to_string(),
            ));
        }
        if self.stack.len() >= MAX_DEPTH {
            return Err(CompileError::Graph(format!(
                "source dependency depth exceeds {MAX_DEPTH}: {}",
                source.id
            )));
        }
        if self.inputs.len() >= MAX_MODULES {
            return Err(CompileError::Graph(format!(
                "source dependency count exceeds {MAX_MODULES}"
            )));
        }
        self.source_bytes = self
            .source_bytes
            .checked_add(source.text.len())
            .filter(|size| *size <= MAX_SOURCE_BYTES)
            .ok_or_else(|| {
                CompileError::Graph(format!("source graph exceeds {MAX_SOURCE_BYTES} bytes"))
            })?;
        let file = crate::parse_file(&source.text).map_err(|error| CompileError::Parse {
            source_id: source.id.clone(),
            detail: error.to_string(),
        })?;
        if !entry
            && (!file.routes.is_empty()
                || file.default_route.is_some()
                || !file.lifecycles.is_empty())
        {
            return Err(CompileError::Graph(format!(
                "dependency source {} contains `route`, `default_route`, or `on`; only entry files may declare them",
                source.id
            )));
        }
        let uses = file.uses.clone();
        let id = ModuleId(self.inputs.len());
        self.by_id.insert(source.id.clone(), id);
        self.inputs.push(ModuleInput {
            source_id: source.id.clone(),
            display_name: source.id.clone(),
            source: source.text,
            file,
            dependencies: BTreeMap::new(),
        });
        self.stack.push(source.id.clone());
        for use_decl in uses {
            let dependency = self
                .resolver
                .resolve(&source.id, &use_decl.source)
                .map_err(|error| CompileError::Source {
                    importer: source.id.clone(),
                    specifier: use_decl.source.clone(),
                    detail: format!("{error:#}"),
                })?;
            let target = self.load(dependency, false)?;
            self.inputs[id.0]
                .dependencies
                .insert(use_decl.source, target);
        }
        self.stack.pop();
        Ok(id)
    }
}

fn ensure_unbound(module: &LinkedModule, name: &str) -> Result<(), LinkError> {
    if module.names.contains_key(name) || module.namespaces.contains_key(name) {
        return Err(LinkError::new(format!(
            "{}: duplicate local binding `{name}`",
            module.input.display_name
        )));
    }
    Ok(())
}

#[cfg(all(test, feature = "syntax"))]
mod tests {
    use super::*;
    use crate::ast::{FlowRef, Ident, Span};

    struct Sources(BTreeMap<(String, String), Source>);

    impl SourceResolver for Sources {
        type Error = String;

        fn resolve(&self, importer_id: &str, specifier: &str) -> Result<Source, Self::Error> {
            self.0
                .get(&(importer_id.to_string(), specifier.to_string()))
                .cloned()
                .ok_or_else(|| format!("missing {specifier}"))
        }
    }

    #[test]
    fn text_compiler_links_public_flows_without_host_ast_interpretation() {
        let entry = Source::new(
            "entry.at",
            "use \"./lib.at\"::{visible as imported}\nflow start() -> string { return subflow(imported) }",
        );
        let sources = Sources(BTreeMap::from([(
            ("entry.at".to_string(), "./lib.at".to_string()),
            Source::new(
                "lib.at",
                "flow hidden() {}\npub flow visible() -> string { return \"ok\" }",
            ),
        )]));
        let program = LinkedProgram::compile(entry, &sources).unwrap();
        let start = program.entry_flow("start").unwrap();
        let imported = FlowRef::Local(Ident::new("imported", Span::default()));
        let resolved = program.resolve(start.module, &imported).unwrap();
        assert_eq!(resolved.name, "visible");
        assert_ne!(resolved.module, start.module);
        assert!(program.entry_flow("imported").is_none());
    }

    #[test]
    fn text_compiler_rejects_private_imports() {
        let entry = Source::new("entry.at", "use \"./lib.at\"::hidden\nflow start() {}");
        let sources = Sources(BTreeMap::from([(
            ("entry.at".to_string(), "./lib.at".to_string()),
            Source::new("lib.at", "flow hidden() {}"),
        )]));
        let error = LinkedProgram::compile(entry, &sources).unwrap_err();
        assert!(error.to_string().contains("private"));
    }
}
