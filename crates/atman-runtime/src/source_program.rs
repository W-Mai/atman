//! Static loading and name resolution for `.at` source dependencies.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use atman_rt::ast::{
    Arg, Expr, File, FlowDecl, FlowRef, Node, Stmt, UseBinding, UseDecl, WatchAction,
};
use serde::{Deserialize, Serialize};

const MAX_MODULES: usize = 128;
const MAX_DEPTH: usize = 32;
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct SourceRoots {
    pub project_root: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
}

/// A revision's source files, identified without machine-specific absolute paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionBundle {
    pub entry: String,
    pub sources: BTreeMap<String, String>,
    #[serde(default)]
    pub edges: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModuleId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowId {
    pub module: ModuleId,
    pub name: String,
}

impl FlowId {
    /// A key disjoint from every DSL identifier, suitable for existing string-keyed maps.
    pub fn runtime_key(&self) -> String {
        format!("@{}:{}", self.module.0, self.name)
    }
}

#[derive(Debug, Clone)]
struct SourceScope {
    label: &'static str,
    root: PathBuf,
    logical_root: PathBuf,
}

#[derive(Debug, Clone)]
struct LinkedModule {
    path: Option<PathBuf>,
    scope: Option<SourceScope>,
    source_id: String,
    source: String,
    file: File,
    names: HashMap<String, FlowId>,
    namespaces: HashMap<String, ModuleId>,
    dependencies: HashMap<String, ModuleId>,
    public: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct LinkedProgram {
    modules: Vec<LinkedModule>,
    entry: ModuleId,
    digest: String,
}

impl LinkedProgram {
    pub fn entry_file(&self) -> &File {
        &self.modules[self.entry.0].file
    }

    pub fn entry_source(&self) -> &str {
        &self.modules[self.entry.0].source
    }

    pub fn entry_path(&self) -> Option<&Path> {
        self.modules[self.entry.0].path.as_deref()
    }

    pub fn entry_module(&self) -> ModuleId {
        self.entry
    }

    /// Entry selection only sees declarations in the entry file, never imported names.
    pub fn entry_flow(&self, name: &str) -> Option<FlowId> {
        self.entry_file()
            .flows
            .iter()
            .find(|flow| flow.name.name == name)
            .map(|_| FlowId {
                module: self.entry,
                name: name.to_owned(),
            })
    }

    /// Resolve a call in its declaring module's lexical scope.
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
            .file
            .flows
            .iter()
            .find(|flow| flow.name.name == id.name)
    }

    pub fn source_dir(&self, id: &FlowId) -> Option<&Path> {
        self.modules.get(id.module.0)?.path.as_deref()?.parent()
    }

    pub fn source_path(&self, id: &FlowId) -> Option<&Path> {
        self.modules.get(id.module.0)?.path.as_deref()
    }

    pub fn flatten_flows(&self) -> HashMap<String, FlowDecl> {
        self.iter_flows()
            .map(|(id, flow)| (id.runtime_key(), flow.clone()))
            .collect()
    }

    pub fn iter_flows(&self) -> impl Iterator<Item = (FlowId, &FlowDecl)> {
        self.modules.iter().enumerate().flat_map(|(index, module)| {
            module.file.flows.iter().map(move |flow| {
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

    pub fn iter_modules(&self) -> impl Iterator<Item = (Option<&Path>, &File)> {
        self.modules
            .iter()
            .map(|module| (module.path.as_deref(), &module.file))
    }

    pub fn closure_digest(&self) -> &str {
        &self.digest
    }

    /// Attach a synthetic entry flow, such as a lifecycle hook, in the entry module's scope.
    /// The source closure and digest remain those of the loaded program.
    pub fn with_entry_flow(&self, flow: FlowDecl) -> Result<Self> {
        let mut program = self.clone();
        let module = &mut program.modules[program.entry.0];
        let name = flow.name.name.clone();
        let path = module
            .path
            .as_deref()
            .map_or_else(|| "<memory>".to_owned(), |p| p.display().to_string());
        ensure_unbound(module, &name, Path::new(&path))?;
        module.names.insert(
            name.clone(),
            FlowId {
                module: program.entry,
                name,
            },
        );
        module.file.flows.push(flow);
        program.validate_calls()?;
        Ok(program)
    }

    /// Canonical source paths and exact text loaded before execution.
    pub fn source_bundle(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.modules
            .iter()
            .filter_map(|module| Some((module.path.as_deref()?, module.source.as_str())))
    }

    pub fn revision_bundle(&self) -> RevisionBundle {
        let sources = self
            .modules
            .iter()
            .map(|module| (logical_id(module), module.source.clone()))
            .collect();
        let edges = self
            .modules
            .iter()
            .filter(|module| !module.dependencies.is_empty())
            .map(|module| {
                (
                    logical_id(module),
                    module
                        .dependencies
                        .iter()
                        .map(|(source, target)| {
                            (source.clone(), logical_id(&self.modules[target.0]))
                        })
                        .collect(),
                )
            })
            .collect();
        RevisionBundle {
            entry: logical_id(&self.modules[self.entry.0]),
            sources,
            edges,
        }
    }

    fn resolve_checked(&self, caller: ModuleId, target: &FlowRef) -> Result<FlowId> {
        if let Some(id) = self.resolve(caller, target) {
            return Ok(id);
        }
        let module = &self.modules[caller.0];
        match target {
            FlowRef::Local(name) if module.namespaces.contains_key(&name.name) => {
                bail!(
                    "`{}` is a module alias; use `{}.flow`",
                    name.name,
                    name.name
                )
            }
            FlowRef::Local(name) => bail!("undefined flow `{}`", name.name),
            FlowRef::Qualified {
                module: alias,
                flow,
            } => {
                let Some(target_id) = module.namespaces.get(&alias.name) else {
                    bail!("undefined module alias `{}`", alias.name);
                };
                let target_module = &self.modules[target_id.0];
                let target_path = target_module
                    .path
                    .as_deref()
                    .map_or_else(|| "<memory>".to_owned(), |p| p.display().to_string());
                if target_module
                    .file
                    .flows
                    .iter()
                    .any(|decl| decl.name.name == flow.name)
                {
                    bail!(
                        "flow `{}` in {} is private; declare it as `pub flow`",
                        flow.name,
                        target_path
                    );
                }
                bail!("flow `{}` does not exist in {}", flow.name, target_path)
            }
        }
    }

    fn validate_calls(&self) -> Result<()> {
        for (index, module) in self.modules.iter().enumerate() {
            let id = ModuleId(index);
            let path = module
                .path
                .as_deref()
                .map_or_else(|| "<memory>".to_owned(), |p| p.display().to_string());
            for flow in &module.file.flows {
                for param in &flow.params {
                    if let Some(default) = &param.default {
                        self.check_expr(id, default).with_context(|| {
                            format!("{}: flow `{}` parameter default", path, flow.name.name)
                        })?;
                    }
                }
                if let Some(contract) = &flow.contract {
                    for block in &contract.blocks {
                        for (_, expr) in &block.kwargs {
                            self.check_expr(id, expr).with_context(|| {
                                format!("{}: flow `{}` contract", path, flow.name.name)
                            })?;
                        }
                    }
                }
                self.check_stmts(id, &flow.body)
                    .with_context(|| format!("{}: flow `{}`", path, flow.name.name))?;
            }
            for lifecycle in &module.file.lifecycles {
                self.check_stmts(id, &lifecycle.body)
                    .with_context(|| format!("{}: lifecycle {:?}", path, lifecycle.event))?;
            }
        }
        Ok(())
    }

    fn check_stmts(&self, caller: ModuleId, stmts: &[Stmt]) -> Result<()> {
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

    fn check_args(&self, caller: ModuleId, args: &[Arg]) -> Result<()> {
        for arg in args {
            match arg {
                Arg::Positional(expr) | Arg::Named { value: expr, .. } => {
                    self.check_expr(caller, expr)?;
                }
            }
        }
        Ok(())
    }

    fn check_expr(&self, caller: ModuleId, expr: &Expr) -> Result<()> {
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
                    self.resolve_checked(caller, name)
                        .with_context(|| format!("subflow({})", name.display_name()))?;
                    self.check_args(caller, args)?;
                }
                Node::ToolCall { args, .. } | Node::Message { args, .. } => {
                    self.check_args(caller, args)?;
                }
                Node::Fanout { source } => self.check_expr(caller, source)?,
                Node::DynamicFanout { source, lambda, .. } => {
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

/// Load all reachable source files, then resolve every flow call before execution.
pub fn load_program(entry_path: impl AsRef<Path>, roots: &SourceRoots) -> Result<LinkedProgram> {
    let entry = fs::canonicalize(entry_path.as_ref())
        .with_context(|| format!("entry source {}", entry_path.as_ref().display()))?;
    if !is_at_file(&entry) {
        bail!("entry source must be an .at file: {}", entry.display());
    }
    let canonical_roots = CanonicalRoots::new(roots)?;
    ensure_entry_root(entry_path.as_ref(), &entry, roots, &canonical_roots)?;
    let scope = canonical_roots.entry_scope(&entry);
    let mut loader = Loader {
        roots: canonical_roots,
        modules: Vec::new(),
        by_path: HashMap::new(),
        stack: Vec::new(),
        source_bytes: 0,
    };
    let entry_id = loader.load(entry, scope, true)?;
    let mut program = LinkedProgram {
        modules: loader.modules,
        entry: entry_id,
        digest: String::new(),
    };
    program.validate_calls()?;
    program.digest = digest(&program.modules);
    Ok(program)
}

fn ensure_entry_root(
    requested: &Path,
    resolved: &Path,
    configured: &SourceRoots,
    canonical: &CanonicalRoots,
) -> Result<()> {
    let requested = absolute_path(requested)?;
    for (label, configured, canonical) in [
        (
            "project",
            configured.project_root.as_deref(),
            canonical.project.as_deref(),
        ),
        (
            "user",
            configured.config_dir.as_deref(),
            canonical.config.as_deref(),
        ),
    ] {
        if let (Some(configured), Some(canonical)) = (configured, canonical) {
            let configured = absolute_path(configured)?;
            if (requested.starts_with(&configured) || requested.starts_with(canonical))
                && !resolved.starts_with(canonical)
            {
                bail!(
                    "entry source {} escapes {} root {} (resolved to {})",
                    requested.display(),
                    label,
                    canonical.display(),
                    resolved.display()
                );
            }
        }
    }
    Ok(())
}

/// Build a linked program without filesystem I/O for existing in-memory callers.
pub fn link_inline(file: File) -> Result<LinkedProgram> {
    if !file.uses.is_empty() {
        bail!("`use` requires a source file path");
    }
    let source = atman_dsl::print::print_file(&file);
    let module = make_module(ModuleId(0), None, None, source, file)?;
    let mut program = LinkedProgram {
        modules: vec![module],
        entry: ModuleId(0),
        digest: String::new(),
    };
    program.validate_calls()?;
    program.digest = digest(&program.modules);
    Ok(program)
}

/// Link recorded source text without reading `.at` files from the current checkout.
pub fn load_program_from_bundle(
    bundle: &RevisionBundle,
    entry_path: impl AsRef<Path>,
    roots: &SourceRoots,
) -> Result<LinkedProgram> {
    if !bundle.sources.contains_key(&bundle.entry) {
        bail!("revision bundle has no entry source `{}`", bundle.entry);
    }
    let entry_path = absolute_path(entry_path.as_ref())?;
    let entry_parent = entry_path
        .parent()
        .ok_or_else(|| anyhow!("entry path has no parent: {}", entry_path.display()))?
        .to_owned();
    let mut loader = BundleLoader {
        bundle,
        project_root: roots
            .project_root
            .as_deref()
            .map(absolute_path)
            .transpose()?,
        config_dir: roots.config_dir.as_deref().map(absolute_path).transpose()?,
        entry_parent,
        entry_path,
        modules: Vec::new(),
        by_id: HashMap::new(),
        stack: Vec::new(),
        source_bytes: 0,
    };
    let entry = loader.load(&bundle.entry, true)?;
    if loader.modules.len() != bundle.sources.len() {
        bail!("revision bundle contains unreachable source files");
    }
    let mut program = LinkedProgram {
        modules: loader.modules,
        entry,
        digest: String::new(),
    };
    program.validate_calls()?;
    program.digest = digest(&program.modules);
    Ok(program)
}

struct BundleLoader<'a> {
    bundle: &'a RevisionBundle,
    project_root: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    entry_parent: PathBuf,
    entry_path: PathBuf,
    modules: Vec<LinkedModule>,
    by_id: HashMap<String, ModuleId>,
    stack: Vec<String>,
    source_bytes: usize,
}

impl BundleLoader<'_> {
    fn load(&mut self, source_id: &str, entry: bool) -> Result<ModuleId> {
        if self.stack.iter().any(|member| member == source_id) {
            let chain = self
                .stack
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(source_id))
                .collect::<Vec<_>>()
                .join(" -> ");
            bail!("source dependency cycle: {chain}");
        }
        if let Some(id) = self.by_id.get(source_id).copied() {
            return Ok(id);
        }
        if self.stack.len() >= MAX_DEPTH {
            bail!("source dependency depth exceeds {MAX_DEPTH}: {source_id}");
        }
        if self.modules.len() >= MAX_MODULES {
            bail!("source dependency count exceeds {MAX_MODULES}");
        }
        let source = self
            .bundle
            .sources
            .get(source_id)
            .ok_or_else(|| anyhow!("revision bundle is missing source `{source_id}`"))?
            .clone();
        self.source_bytes = self
            .source_bytes
            .checked_add(source.len())
            .filter(|size| *size <= MAX_SOURCE_BYTES)
            .ok_or_else(|| anyhow!("source graph exceeds {MAX_SOURCE_BYTES} bytes"))?;
        let (path, scope) = self.source_path(source_id, entry)?;
        let file = atman_dsl::parse::parse_file(&source)
            .with_context(|| format!("parse recorded source `{source_id}`"))?;
        if !entry
            && (!file.routes.is_empty()
                || file.default_route.is_some()
                || !file.lifecycles.is_empty())
        {
            bail!(
                "dependency source `{source_id}` contains `route`, `default_route`, or `on`; only entry files may declare them"
            );
        }
        let uses = file.uses.clone();
        let id = ModuleId(self.modules.len());
        let mut module = make_module(id, Some(path), Some(scope), source, file)?;
        module.source_id = source_id.to_owned();
        self.modules.push(module);
        self.by_id.insert(source_id.to_owned(), id);
        self.stack.push(source_id.to_owned());
        for use_decl in uses {
            let target = self
                .bundle
                .edges
                .get(source_id)
                .and_then(|edges| edges.get(&use_decl.source))
                .ok_or_else(|| {
                    anyhow!(
                        "recorded source `{source_id}` has no target for use {:?}",
                        use_decl.source
                    )
                })?
                .clone();
            let dependency_id = self.load(&target, false).with_context(|| {
                format!(
                    "recorded source `{source_id}`: use {:?} -> `{target}`",
                    use_decl.source
                )
            })?;
            self.modules[id.0]
                .dependencies
                .insert(use_decl.source.clone(), dependency_id);
            bind_use(
                &mut self.modules,
                id,
                dependency_id,
                &use_decl,
                source_id,
                &target,
            )?;
        }
        self.stack.pop();
        Ok(id)
    }

    fn source_path(&self, source_id: &str, entry: bool) -> Result<(PathBuf, SourceScope)> {
        let (kind, relative) = source_id
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid revision source ID `{source_id}`"))?;
        if relative.is_empty()
            || relative.contains('\\')
            || relative.contains(':')
            || relative
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !is_at_file(Path::new(relative))
        {
            bail!("invalid revision source ID `{source_id}`");
        }
        let (base, label, restricted) = match kind {
            "project" => {
                let base = self
                    .project_root
                    .as_ref()
                    .ok_or_else(|| anyhow!("revision requires a project root"))?;
                (base, "project", relative.starts_with(".atman/lib/"))
            }
            "user" => {
                let base = self
                    .config_dir
                    .as_ref()
                    .ok_or_else(|| anyhow!("revision requires a user config directory"))?;
                (base, "user", relative.starts_with("lib/"))
            }
            "entry" => (&self.entry_parent, "ad-hoc", false),
            _ => bail!("invalid revision source ID `{source_id}`"),
        };
        let path = if entry {
            self.entry_path.clone()
        } else {
            base.join(relative)
        };
        let (label, root) = if restricted && kind == "project" {
            ("project-lib", base.join(".atman/lib"))
        } else if restricted {
            ("user-lib", base.join("lib"))
        } else {
            (label, base.clone())
        };
        Ok((
            path,
            SourceScope {
                label,
                root,
                logical_root: base.clone(),
            },
        ))
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[derive(Debug)]
struct CanonicalRoots {
    project: Option<PathBuf>,
    config: Option<PathBuf>,
}

impl CanonicalRoots {
    fn new(roots: &SourceRoots) -> Result<Self> {
        let project = roots
            .project_root
            .as_deref()
            .map(fs::canonicalize)
            .transpose()
            .context("project source root")?;
        let config = roots
            .config_dir
            .as_deref()
            .filter(|path| path.exists())
            .map(fs::canonicalize)
            .transpose()
            .context("user source root")?;
        Ok(Self { project, config })
    }

    fn entry_scope(&self, entry: &Path) -> SourceScope {
        if let Some(root) = &self.project {
            if entry.starts_with(root) {
                return self.narrow_library_scope(
                    entry,
                    SourceScope {
                        label: "project",
                        root: root.clone(),
                        logical_root: root.clone(),
                    },
                );
            }
        }
        if let Some(root) = &self.config {
            if entry.starts_with(root) {
                return self.narrow_library_scope(
                    entry,
                    SourceScope {
                        label: "user",
                        root: root.clone(),
                        logical_root: root.clone(),
                    },
                );
            }
        }
        let root = entry
            .parent()
            .expect("canonical file has parent")
            .to_owned();
        SourceScope {
            label: "ad-hoc",
            root: root.clone(),
            logical_root: root,
        }
    }

    fn narrow_library_scope(&self, path: &Path, scope: SourceScope) -> SourceScope {
        for label in ["project", "user"] {
            if let Ok(library) = self.library(label) {
                if path.starts_with(&library.root) {
                    return library;
                }
            }
        }
        scope
    }

    fn library(&self, label: &'static str) -> Result<SourceScope> {
        let base = match label {
            "project" => self
                .project
                .as_ref()
                .ok_or_else(|| anyhow!("`project:` source requires a project root"))?,
            "user" => self
                .config
                .as_ref()
                .ok_or_else(|| anyhow!("`user:` source requires a user config directory"))?,
            _ => unreachable!(),
        };
        let candidate = if label == "project" {
            base.join(".atman/lib")
        } else {
            base.join("lib")
        };
        let root = fs::canonicalize(&candidate)
            .with_context(|| format!("{} library root {}", label, candidate.display()))?;
        if !root.starts_with(base) {
            bail!(
                "{} library root {} escapes {}",
                label,
                candidate.display(),
                base.display()
            );
        }
        Ok(SourceScope {
            label: if label == "project" {
                "project-lib"
            } else {
                "user-lib"
            },
            root,
            logical_root: base.clone(),
        })
    }
}

struct Loader {
    roots: CanonicalRoots,
    modules: Vec<LinkedModule>,
    by_path: HashMap<PathBuf, ModuleId>,
    stack: Vec<PathBuf>,
    source_bytes: usize,
}

impl Loader {
    fn load(&mut self, path: PathBuf, scope: SourceScope, entry: bool) -> Result<ModuleId> {
        if self.stack.contains(&path) {
            let chain = self
                .stack
                .iter()
                .chain(std::iter::once(&path))
                .map(|member| member.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            bail!("source dependency cycle: {chain}");
        }
        if let Some(id) = self.by_path.get(&path).copied() {
            let loaded_scope = self.modules[id.0].scope.as_ref().expect("file scope");
            if loaded_scope.root != scope.root {
                bail!(
                    "source {} reached under different roots ({} and {})",
                    path.display(),
                    loaded_scope.root.display(),
                    scope.root.display()
                );
            }
            return Ok(id);
        }
        if self.stack.len() >= MAX_DEPTH {
            bail!(
                "source dependency depth exceeds {MAX_DEPTH}: {}",
                path.display()
            );
        }
        if self.modules.len() >= MAX_MODULES {
            bail!("source dependency count exceeds {MAX_MODULES}");
        }
        let metadata = fs::metadata(&path).with_context(|| format!("source {}", path.display()))?;
        if !metadata.is_file() {
            bail!("source is not a file: {}", path.display());
        }
        if metadata.len() > (MAX_SOURCE_BYTES - self.source_bytes) as u64 {
            bail!(
                "source graph exceeds {MAX_SOURCE_BYTES} bytes at {}",
                path.display()
            );
        }
        let bytes = fs::read(&path).with_context(|| format!("source {}", path.display()))?;
        self.source_bytes = self
            .source_bytes
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_SOURCE_BYTES)
            .ok_or_else(|| anyhow!("source graph exceeds {MAX_SOURCE_BYTES} bytes"))?;
        let source = String::from_utf8(bytes)
            .with_context(|| format!("source {} is not UTF-8", path.display()))?;
        let file = atman_dsl::parse::parse_file(&source)
            .with_context(|| format!("parse source {}", path.display()))?;
        if !entry
            && (!file.routes.is_empty()
                || file.default_route.is_some()
                || !file.lifecycles.is_empty())
        {
            bail!(
                "dependency source {} contains `route`, `default_route`, or `on`; only entry files may declare them",
                path.display()
            );
        }
        let uses = file.uses.clone();
        let id = ModuleId(self.modules.len());
        self.modules.push(make_module(
            id,
            Some(path.clone()),
            Some(scope.clone()),
            source,
            file,
        )?);
        self.by_path.insert(path.clone(), id);
        self.stack.push(path.clone());
        for use_decl in uses {
            let (dependency, dependency_scope) = self
                .resolve_source(&path, &scope, &use_decl.source)
                .with_context(|| format!("{}: use {:?}", path.display(), use_decl.source))?;
            let dependency_id = self
                .load(dependency.clone(), dependency_scope, false)
                .with_context(|| {
                    format!(
                        "{}: use {:?} resolved to {}",
                        path.display(),
                        use_decl.source,
                        dependency.display()
                    )
                })?;
            self.modules[id.0]
                .dependencies
                .insert(use_decl.source.clone(), dependency_id);
            bind_use(
                &mut self.modules,
                id,
                dependency_id,
                &use_decl,
                &path.display().to_string(),
                &dependency.display().to_string(),
            )?;
        }
        self.stack.pop();
        Ok(id)
    }

    fn resolve_source(
        &self,
        caller: &Path,
        scope: &SourceScope,
        specifier: &str,
    ) -> Result<(PathBuf, SourceScope)> {
        if specifier.contains('\\') {
            bail!("source path must use `/` separators: {specifier:?}");
        }
        let (candidate, selected_scope) = if let Some(rest) = specifier.strip_prefix("project:") {
            let selected = self.roots.library("project")?;
            (selected.root.join(valid_library_path(rest)?), selected)
        } else if let Some(rest) = specifier.strip_prefix("user:") {
            let selected = self.roots.library("user")?;
            (selected.root.join(valid_library_path(rest)?), selected)
        } else if specifier.starts_with("./") || specifier.starts_with("../") {
            (
                caller
                    .parent()
                    .expect("canonical file has parent")
                    .join(specifier),
                scope.clone(),
            )
        } else {
            bail!("source must start with `./`, `../`, `project:`, or `user:`: {specifier:?}");
        };
        if !is_at_file(&candidate) {
            bail!("source must end in `.at`: {specifier:?}");
        }
        let path = fs::canonicalize(&candidate)
            .with_context(|| format!("source candidate {}", candidate.display()))?;
        if !path.starts_with(&selected_scope.root) {
            bail!(
                "source candidate {} escapes {} root {} (resolved to {})",
                candidate.display(),
                selected_scope.label,
                selected_scope.root.display(),
                path.display()
            );
        }
        let selected_scope = self.roots.narrow_library_scope(&path, selected_scope);
        Ok((path, selected_scope))
    }
}

fn valid_library_path(rest: &str) -> Result<&Path> {
    if rest.is_empty() || rest.starts_with('/') || rest.contains(':') {
        bail!("invalid library source path {rest:?}");
    }
    Ok(Path::new(rest))
}

fn is_at_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "at")
}

fn make_module(
    id: ModuleId,
    path: Option<PathBuf>,
    scope: Option<SourceScope>,
    source: String,
    file: File,
) -> Result<LinkedModule> {
    let source_id = source_id_for(path.as_deref(), scope.as_ref());
    let mut names = HashMap::new();
    let mut declared = HashSet::new();
    let location = path
        .as_deref()
        .map_or_else(|| "<memory>".to_owned(), |p| p.display().to_string());
    for flow in &file.flows {
        if !declared.insert(flow.name.name.clone()) {
            bail!("{}: duplicate flow `{}`", location, flow.name.name);
        }
        names.insert(
            flow.name.name.clone(),
            FlowId {
                module: id,
                name: flow.name.name.clone(),
            },
        );
    }
    let mut public = HashSet::new();
    for name in &file.public_flows {
        if !declared.contains(&name.name) {
            bail!("{}: `pub flow {}` is not declared", location, name.name);
        }
        if !public.insert(name.name.clone()) {
            bail!("{}: duplicate `pub flow {}`", location, name.name);
        }
    }
    Ok(LinkedModule {
        path,
        scope,
        source_id,
        source,
        file,
        names,
        namespaces: HashMap::new(),
        dependencies: HashMap::new(),
        public,
    })
}

fn ensure_unbound(module: &LinkedModule, name: &str, path: &Path) -> Result<()> {
    if module.names.contains_key(name) || module.namespaces.contains_key(name) {
        bail!("{}: duplicate local binding `{}`", path.display(), name);
    }
    Ok(())
}

fn bind_use(
    modules: &mut [LinkedModule],
    caller: ModuleId,
    dependency: ModuleId,
    use_decl: &UseDecl,
    caller_path: &str,
    dependency_path: &str,
) -> Result<()> {
    match &use_decl.binding {
        UseBinding::Module(alias) => {
            let module = &mut modules[caller.0];
            ensure_unbound(module, &alias.name, Path::new(caller_path))?;
            module.namespaces.insert(alias.name.clone(), dependency);
        }
        UseBinding::Flows(bindings) => {
            for binding in bindings {
                let dependency_module = &modules[dependency.0];
                if !dependency_module.public.contains(&binding.name.name) {
                    if dependency_module
                        .file
                        .flows
                        .iter()
                        .any(|flow| flow.name.name == binding.name.name)
                    {
                        bail!(
                            "{}: use {:?}: flow `{}` in {} is private; declare it as `pub flow`",
                            caller_path,
                            use_decl.source,
                            binding.name.name,
                            dependency_path
                        );
                    }
                    bail!(
                        "{}: use {:?}: flow `{}` does not exist in {}",
                        caller_path,
                        use_decl.source,
                        binding.name.name,
                        dependency_path
                    );
                }
                let local_name = binding.alias.as_ref().unwrap_or(&binding.name).name.clone();
                let module = &mut modules[caller.0];
                ensure_unbound(module, &local_name, Path::new(caller_path))?;
                module.names.insert(
                    local_name,
                    FlowId {
                        module: dependency,
                        name: binding.name.name.clone(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn logical_id(module: &LinkedModule) -> String {
    module.source_id.clone()
}

fn source_id_for(path: Option<&Path>, scope: Option<&SourceScope>) -> String {
    match (path, scope) {
        (Some(path), Some(scope)) => {
            let prefix = match scope.label {
                "project" | "project-lib" => "project",
                "user" | "user-lib" => "user",
                _ => "entry",
            };
            let relative = path.strip_prefix(&scope.logical_root).unwrap_or(path);
            format!(
                "{}:{}",
                prefix,
                relative.to_string_lossy().replace('\\', "/")
            )
        }
        _ => "entry:inline.at".to_owned(),
    }
}

fn digest(modules: &[LinkedModule]) -> String {
    let mut sources = modules
        .iter()
        .map(|module| (logical_id(module), module.source.as_bytes()))
        .collect::<Vec<_>>();
    sources.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = blake3::Hasher::new();
    for (id, source) in sources {
        hasher.update(&(id.len() as u64).to_le_bytes());
        hasher.update(id.as_bytes());
        hasher.update(&(source.len() as u64).to_le_bytes());
        hasher.update(source);
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_rt::ast::{Ident, Span};
    use tempfile::TempDir;

    fn write(root: &Path, relative: &str, source: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        path
    }

    fn local(name: &str) -> FlowRef {
        FlowRef::Local(Ident::new(name, Span::default()))
    }

    fn qualified(module: &str, flow: &str) -> FlowRef {
        FlowRef::Qualified {
            module: Ident::new(module, Span::default()),
            flow: Ident::new(flow, Span::default()),
        }
    }

    #[test]
    fn nested_bindings_are_lexical_and_preserve_source_provenance() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "lib/text.at",
            "flow helper() -> string { return \"ok\" }\npub flow normalize() -> string { return subflow(helper) }",
        );
        write(
            dir.path(),
            "lib/wrapper.at",
            "use \"./text.at\"::normalize as inner\npub flow wrapped() -> string { return subflow(inner) }",
        );
        let entry = write(
            dir.path(),
            "entry.at",
            "use \"./lib/wrapper.at\"::{wrapped as word}\nuse \"./lib/text.at\" as text\nflow start() -> string { result = subflow(word) return subflow(text.normalize) }",
        );
        let program = load_program(&entry, &SourceRoots::default()).unwrap();
        let start = program.entry_flow("start").unwrap();
        assert!(program.entry_flow("word").is_none());
        let wrapped = program.resolve(start.module, &local("word")).unwrap();
        assert_eq!(wrapped.name, "wrapped");
        let normalize = program
            .resolve(start.module, &qualified("text", "normalize"))
            .unwrap();
        assert_ne!(wrapped.module, normalize.module);
        let inner = program.resolve(wrapped.module, &local("inner")).unwrap();
        assert_eq!(inner, normalize);
        let helper = program.resolve(normalize.module, &local("helper")).unwrap();
        assert_eq!(helper.module, normalize.module);
        assert!(program.resolve(start.module, &local("helper")).is_none());
        assert!(
            program
                .resolve(start.module, &qualified("text", "helper"))
                .is_none()
        );
        assert_eq!(program.flatten_flows().len(), 4);
        let expected_dir = fs::canonicalize(dir.path().join("lib")).unwrap();
        assert_eq!(program.source_dir(&normalize), Some(expected_dir.as_path()));
        assert_eq!(program.source_bundle().count(), 3);
        assert_eq!(program.entry_source(), fs::read_to_string(&entry).unwrap());
    }

    #[test]
    fn private_missing_and_duplicate_bindings_are_link_errors() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "lib.at",
            "flow hidden() {}\npub flow visible() {}",
        );
        let entry = write(
            dir.path(),
            "entry.at",
            "use \"./lib.at\"::hidden\nflow start() {}",
        );
        let error = load_program(&entry, &SourceRoots::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("private"), "{error}");
        assert!(error.contains("pub flow"), "{error}");

        fs::write(
            &entry,
            "use \"./lib.at\" as lib\nflow start() { return subflow(lib.hidden) }",
        )
        .unwrap();
        let error = load_program(&entry, &SourceRoots::default()).unwrap_err();
        assert!(format!("{error:#}").contains("private"));

        fs::write(&entry, "use \"./lib.at\"::missing\nflow start() {}").unwrap();
        let error = load_program(&entry, &SourceRoots::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not exist"), "{error}");

        fs::write(
            &entry,
            "use \"./lib.at\"::{visible as start}\nflow start() {}",
        )
        .unwrap();
        let error = load_program(&entry, &SourceRoots::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate local binding"), "{error}");
    }

    #[test]
    fn cycles_and_dependency_side_lifecycle_are_rejected() {
        let dir = TempDir::new().unwrap();
        let entry = write(dir.path(), "a.at", "use \"./b.at\" as b\nflow start() {}");
        write(
            dir.path(),
            "b.at",
            "use \"./a.at\" as a\npub flow worker() {}",
        );
        let error = load_program(&entry, &SourceRoots::default()).unwrap_err();
        let detail = format!("{error:#}");
        assert!(detail.contains("cycle"), "{detail}");
        assert!(
            detail.contains("a.at") && detail.contains("b.at"),
            "{detail}"
        );

        fs::write(
            dir.path().join("b.at"),
            "on session.start { return \"x\" }\npub flow worker() {}",
        )
        .unwrap();
        let error = load_program(&entry, &SourceRoots::default()).unwrap_err();
        assert!(format!("{error:#}").contains("only entry files"));
    }

    #[test]
    fn relative_paths_cannot_escape_the_selected_source_root() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "outside.at", "pub flow outside() {}");
        let entry = write(
            dir.path(),
            "entry/main.at",
            "use \"../outside.at\" as outside\nflow start() {}",
        );
        let error = load_program(&entry, &SourceRoots::default()).unwrap_err();
        assert!(format!("{error:#}").contains("escapes"));

        fs::write(&entry, "use \"./missing.at\" as missing\nflow start() {}").unwrap();
        let error = load_program(&entry, &SourceRoots::default()).unwrap_err();
        let detail = format!("{error:#}");
        assert!(detail.contains("missing.at") && detail.contains("entry/main.at"));
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinks_cannot_escape_root() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let outside = write(dir.path(), "outside.at", "pub flow outside() {}");
        let entry = write(
            dir.path(),
            "entry/main.at",
            "use \"./link.at\" as link\nflow start() {}",
        );
        symlink(outside, dir.path().join("entry/link.at")).unwrap();
        let error = load_program(&entry, &SourceRoots::default()).unwrap_err();
        assert!(format!("{error:#}").contains("escapes"));
    }

    #[cfg(unix)]
    #[test]
    fn entry_symlink_cannot_reclassify_a_project_source_as_ad_hoc() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let outside = write(dir.path(), "outside.at", "flow start() {}");
        let entry = project.join("entry.at");
        symlink(outside, &entry).unwrap();
        let roots = SourceRoots {
            project_root: Some(project),
            config_dir: None,
        };
        let error = load_program(&entry, &roots).unwrap_err();
        assert!(format!("{error:#}").contains("escapes project root"));
    }

    #[test]
    fn library_roots_and_digest_track_dependency_bytes() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        let config = dir.path().join("config");
        let shared = write(
            &project,
            ".atman/lib/shared.at",
            "pub flow shared() -> string { return \"one\" }",
        );
        write(&config, "lib/user.at", "pub flow user_flow() {}");
        let entry = write(
            &project,
            "commands/main.at",
            "use \"project:shared.at\" as project_lib\nuse \"user:user.at\"::user_flow as named\nflow start() -> string { result = subflow(named) return subflow(project_lib.shared) }",
        );
        let roots = SourceRoots {
            project_root: Some(project.clone()),
            config_dir: Some(config),
        };
        let first = load_program(&entry, &roots).unwrap();
        let start = first.entry_flow("start").unwrap();
        assert!(first.resolve(start.module, &local("named")).is_some());
        assert!(
            first
                .resolve(start.module, &qualified("project_lib", "shared"))
                .is_some()
        );

        fs::write(&shared, "pub flow shared() -> string { return \"two\" }").unwrap();
        let second = load_program(&entry, &roots).unwrap();
        assert_ne!(first.closure_digest(), second.closure_digest());
        assert_eq!(first.entry_source(), second.entry_source());

        fs::write(
            &shared,
            "use \"../outside.at\" as outside\npub flow shared() {}",
        )
        .unwrap();
        write(&project, ".atman/outside.at", "pub flow outside() {}");
        let error = load_program(&entry, &roots).unwrap_err();
        assert!(format!("{error:#}").contains("escapes"));
    }

    #[test]
    fn inline_programs_require_no_source_dependencies() {
        let file = atman_dsl::parse::parse_file("flow start() {}").unwrap();
        let program = link_inline(file).unwrap();
        assert!(program.entry_flow("start").is_some());
        assert!(
            program
                .source_dir(&program.entry_flow("start").unwrap())
                .is_none()
        );
        let file =
            atman_dsl::parse::parse_file("use \"./other.at\" as other\nflow start() {}").unwrap();
        assert!(
            link_inline(file)
                .unwrap_err()
                .to_string()
                .contains("requires a source file path")
        );
    }

    #[test]
    fn revision_bundle_relinks_without_source_files() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        let library = write(
            &project,
            ".atman/lib/helper.at",
            "pub flow answer() -> string { return \"recorded\" }",
        );
        let entry = write(
            &project,
            "commands/main.at",
            "use \"project:helper.at\" as helper\nflow start() -> string { return subflow(helper.answer) }",
        );
        let roots = SourceRoots {
            project_root: Some(project),
            config_dir: None,
        };
        let original = load_program(&entry, &roots).unwrap();
        let bundle = original.revision_bundle();
        assert_eq!(bundle.sources.len(), 2);
        assert!(bundle.sources.contains_key("project:.atman/lib/helper.at"));
        fs::remove_file(&entry).unwrap();
        fs::remove_file(&library).unwrap();

        let replayed = load_program_from_bundle(&bundle, &entry, &roots).unwrap();
        assert_eq!(replayed.closure_digest(), original.closure_digest());
        assert_eq!(replayed.entry_source(), original.entry_source());
        let start = replayed.entry_flow("start").unwrap();
        assert!(
            replayed
                .resolve(start.module, &qualified("helper", "answer"))
                .is_some()
        );
    }
}
