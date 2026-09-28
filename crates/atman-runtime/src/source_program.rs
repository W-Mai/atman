//! Filesystem and revision-bundle adapters for the language compiler.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use atman_rt::ast::{File, FlowDecl, FlowRef, LifecycleEvent};
pub use atman_rt::program::{FlowId, ModuleId};
use atman_rt::program::{
    LinkedProgram as RtLinkedProgram, MAX_SOURCE_BYTES, ModuleInput, Source, SourceResolver,
};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone)]
struct SourceScope {
    label: &'static str,
    root: PathBuf,
    logical_root: PathBuf,
}

#[derive(Debug, Clone)]
struct SourceMeta {
    path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LinkedProgram {
    core: Arc<RtLinkedProgram>,
    sources: Vec<SourceMeta>,
    digest: String,
}

impl LinkedProgram {
    pub fn core(&self) -> &RtLinkedProgram {
        &self.core
    }

    pub fn shared_core(&self) -> Arc<RtLinkedProgram> {
        Arc::clone(&self.core)
    }

    pub fn entry_file(&self) -> &File {
        self.core.entry_file()
    }

    pub fn entry_source(&self) -> &str {
        self.core.entry_source()
    }

    pub fn route(&self, input: &str) -> Option<atman_rt::route::RouteMatch> {
        self.core.route(input)
    }

    pub fn lifecycle_flows(&self, event: LifecycleEvent) -> impl Iterator<Item = FlowDecl> + '_ {
        self.core.lifecycle_flows(event)
    }

    pub fn entry_path(&self) -> Option<&Path> {
        self.sources[self.core.entry_module().0].path.as_deref()
    }

    pub fn entry_module(&self) -> ModuleId {
        self.core.entry_module()
    }

    pub fn entry_flow(&self, name: &str) -> Option<FlowId> {
        self.core.entry_flow(name)
    }

    pub fn resolve(&self, caller: ModuleId, target: &FlowRef) -> Option<FlowId> {
        self.core.resolve(caller, target)
    }

    pub fn flow(&self, id: &FlowId) -> Option<&FlowDecl> {
        self.core.flow(id)
    }

    pub fn source_dir(&self, id: &FlowId) -> Option<&Path> {
        self.source_path(id)?.parent()
    }

    pub fn source_path(&self, id: &FlowId) -> Option<&Path> {
        self.sources.get(id.module.0)?.path.as_deref()
    }

    pub fn flatten_flows(&self) -> HashMap<String, FlowDecl> {
        self.core
            .iter_flows()
            .map(|(id, flow)| (id.runtime_key(), flow.clone()))
            .collect()
    }

    pub fn iter_flows(&self) -> impl Iterator<Item = (FlowId, &FlowDecl)> {
        self.core.iter_flows()
    }

    pub fn iter_modules(&self) -> impl Iterator<Item = (Option<&Path>, &File)> {
        self.core
            .modules()
            .map(|(id, module)| (self.sources[id.0].path.as_deref(), &module.file))
    }

    pub fn closure_digest(&self) -> &str {
        &self.digest
    }

    /// Attach a synthetic entry flow without changing the source closure or digest.
    pub fn with_entry_flow(&self, flow: FlowDecl) -> Result<Self> {
        Ok(Self {
            core: Arc::new(self.core.with_entry_flow(flow)?),
            sources: self.sources.clone(),
            digest: self.digest.clone(),
        })
    }

    /// Canonical source paths and exact text loaded before execution.
    pub fn source_bundle(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.core.modules().filter_map(|(id, module)| {
            Some((self.sources[id.0].path.as_deref()?, module.source.as_str()))
        })
    }

    pub fn revision_bundle(&self) -> RevisionBundle {
        let sources = self
            .core
            .modules()
            .map(|(_, module)| (module.source_id.clone(), module.source.clone()))
            .collect();
        let edges = self
            .core
            .modules()
            .filter(|(_, module)| !module.dependencies.is_empty())
            .map(|(_, module)| {
                let targets = module
                    .dependencies
                    .iter()
                    .map(|(specifier, id)| {
                        (
                            specifier.clone(),
                            self.core
                                .module(*id)
                                .expect("linked dependency")
                                .source_id
                                .clone(),
                        )
                    })
                    .collect();
                (module.source_id.clone(), targets)
            })
            .collect();
        let entry = self
            .core
            .module(self.core.entry_module())
            .expect("linked entry")
            .source_id
            .clone();
        RevisionBundle {
            entry,
            sources,
            edges,
        }
    }
}

fn finish_compiled(
    core: RtLinkedProgram,
    paths: Option<&HashMap<String, PathBuf>>,
) -> Result<LinkedProgram> {
    let digest = digest(&core);
    let sources =
        core.modules()
            .map(|(_, module)| {
                let path = paths
                    .map(|paths| {
                        paths.get(&module.source_id).cloned().ok_or_else(|| {
                            anyhow!("missing path for source `{}`", module.source_id)
                        })
                    })
                    .transpose()?;
                Ok(SourceMeta { path })
            })
            .collect::<Result<Vec<_>>>()?;
    Ok(LinkedProgram {
        core: Arc::new(core),
        sources,
        digest,
    })
}

/// Read a path-controlled source graph and compile it through the language VM.
pub fn load_program(entry_path: impl AsRef<Path>, roots: &SourceRoots) -> Result<LinkedProgram> {
    let entry = fs::canonicalize(entry_path.as_ref())
        .with_context(|| format!("entry source {}", entry_path.as_ref().display()))?;
    if !is_at_file(&entry) {
        bail!("entry source must be an .at file: {}", entry.display());
    }
    let canonical_roots = CanonicalRoots::new(roots)?;
    ensure_entry_root(entry_path.as_ref(), &entry, roots, &canonical_roots)?;
    let scope = canonical_roots.entry_scope(&entry);
    let resolver = FilesystemResolver {
        roots: canonical_roots,
        sources: RefCell::new(HashMap::new()),
    };
    let entry_source = resolver.source(entry, scope)?;
    let core = RtLinkedProgram::compile(entry_source, &resolver)?;
    let paths = resolver
        .sources
        .into_inner()
        .into_iter()
        .map(|(id, source)| (id, source.path))
        .collect();
    finish_compiled(core, Some(&paths))
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

/// Link an AST already held by the host without source dependencies.
pub fn link_inline(file: File) -> Result<LinkedProgram> {
    if !file.uses.is_empty() {
        bail!("`use` requires a source file path");
    }
    let source = atman_rt::print_file(&file);
    let core = RtLinkedProgram::link(
        vec![ModuleInput {
            source_id: "entry:inline.at".to_owned(),
            display_name: "<memory>".to_owned(),
            source,
            file,
            dependencies: BTreeMap::new(),
        }],
        ModuleId(0),
    )?;
    finish_compiled(core, None)
}

/// Compile recorded source text without reading the current checkout.
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
    let resolver = BundleResolver {
        bundle,
        project_root: roots
            .project_root
            .as_deref()
            .map(absolute_path)
            .transpose()?,
        config_dir: roots.config_dir.as_deref().map(absolute_path).transpose()?,
        entry_parent,
        entry_path,
        paths: RefCell::new(HashMap::new()),
    };
    let entry = resolver.source(&bundle.entry, true)?;
    let core = RtLinkedProgram::compile(entry, &resolver)?;
    if core.modules().count() != bundle.sources.len() {
        bail!("revision bundle contains unreachable source files");
    }
    finish_compiled(core, Some(&resolver.paths.into_inner()))
}

struct BundleResolver<'a> {
    bundle: &'a RevisionBundle,
    project_root: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    entry_parent: PathBuf,
    entry_path: PathBuf,
    paths: RefCell<HashMap<String, PathBuf>>,
}

impl BundleResolver<'_> {
    fn source(&self, source_id: &str, entry: bool) -> Result<Source> {
        let text = self
            .bundle
            .sources
            .get(source_id)
            .ok_or_else(|| anyhow!("revision bundle is missing source `{source_id}`"))?;
        if text.len() > MAX_SOURCE_BYTES {
            bail!("source graph exceeds {MAX_SOURCE_BYTES} bytes");
        }
        let path = self.source_path(source_id, entry)?;
        let mut paths = self.paths.borrow_mut();
        if let Some(existing) = paths.get(source_id) {
            if existing != &path {
                bail!("revision source `{source_id}` resolves to different paths");
            }
        } else {
            paths.insert(source_id.to_owned(), path);
        }
        Ok(Source::new(source_id, text.clone()))
    }

    fn source_path(&self, source_id: &str, entry: bool) -> Result<PathBuf> {
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
        let base = match kind {
            "project" => self
                .project_root
                .as_ref()
                .ok_or_else(|| anyhow!("revision requires a project root"))?,
            "user" => self
                .config_dir
                .as_ref()
                .ok_or_else(|| anyhow!("revision requires a user config directory"))?,
            "entry" => &self.entry_parent,
            _ => bail!("invalid revision source ID `{source_id}`"),
        };
        Ok(if entry {
            self.entry_path.clone()
        } else {
            base.join(relative)
        })
    }
}

impl SourceResolver for BundleResolver<'_> {
    type Error = anyhow::Error;

    fn resolve(&self, importer_id: &str, specifier: &str) -> Result<Source> {
        let target = self
            .bundle
            .edges
            .get(importer_id)
            .and_then(|edges| edges.get(specifier))
            .ok_or_else(|| {
                anyhow!("recorded source `{importer_id}` has no target for use {specifier:?}")
            })?;
        self.source(target, false)
    }
}

#[derive(Debug, Clone)]
struct ResolvedSource {
    path: PathBuf,
    scope: SourceScope,
    text: String,
}

struct FilesystemResolver {
    roots: CanonicalRoots,
    sources: RefCell<HashMap<String, ResolvedSource>>,
}

impl FilesystemResolver {
    fn source(&self, path: PathBuf, scope: SourceScope) -> Result<Source> {
        let source_id = source_id_for(Some(&path), Some(&scope));
        let sources = self.sources.borrow();
        if let Some((existing_id, existing)) =
            sources.iter().find(|(_, existing)| existing.path == path)
        {
            if existing.scope.root != scope.root {
                bail!(
                    "source {} reached under different roots ({} and {})",
                    path.display(),
                    existing.scope.root.display(),
                    scope.root.display()
                );
            }
            return Ok(Source::new(existing_id.clone(), existing.text.clone()));
        }
        if let Some(existing) = sources.get(&source_id) {
            bail!(
                "source ID `{source_id}` is shared by {} and {}",
                existing.path.display(),
                path.display()
            );
        }
        drop(sources);
        let text = read_source(&path)?;
        self.sources.borrow_mut().insert(
            source_id.clone(),
            ResolvedSource {
                path,
                scope,
                text: text.clone(),
            },
        );
        Ok(Source::new(source_id, text))
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

impl SourceResolver for FilesystemResolver {
    type Error = anyhow::Error;

    fn resolve(&self, importer_id: &str, specifier: &str) -> Result<Source> {
        let (path, scope) = {
            let sources = self.sources.borrow();
            let importer = sources
                .get(importer_id)
                .ok_or_else(|| anyhow!("unknown importing source `{importer_id}`"))?;
            (importer.path.clone(), importer.scope.clone())
        };
        let (resolved, scope) = self
            .resolve_source(&path, &scope, specifier)
            .with_context(|| format!("{}: use {specifier:?}", path.display()))?;
        self.source(resolved.clone(), scope).with_context(|| {
            format!(
                "{}: use {specifier:?} resolved to {}",
                path.display(),
                resolved.display()
            )
        })
    }
}

fn read_source(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path).with_context(|| format!("source {}", path.display()))?;
    if !metadata.is_file() {
        bail!("source is not a file: {}", path.display());
    }
    if metadata.len() > MAX_SOURCE_BYTES as u64 {
        bail!(
            "source graph exceeds {MAX_SOURCE_BYTES} bytes at {}",
            path.display()
        );
    }
    let bytes = fs::read(path).with_context(|| format!("source {}", path.display()))?;
    if bytes.len() > MAX_SOURCE_BYTES {
        bail!(
            "source graph exceeds {MAX_SOURCE_BYTES} bytes at {}",
            path.display()
        );
    }
    String::from_utf8(bytes).with_context(|| format!("source {} is not UTF-8", path.display()))
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

fn valid_library_path(rest: &str) -> Result<&Path> {
    if rest.is_empty() || rest.starts_with('/') || rest.contains(':') {
        bail!("invalid library source path {rest:?}");
    }
    Ok(Path::new(rest))
}

fn is_at_file(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "at")
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

fn digest(program: &RtLinkedProgram) -> String {
    let mut sources = program
        .modules()
        .map(|(_, module)| (module.source_id.as_str(), module.source.as_bytes()))
        .collect::<Vec<_>>();
    sources.sort_unstable_by(|left, right| left.0.cmp(right.0));
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
        let file = atman_rt::parse_file("flow start() {}").unwrap();
        let program = link_inline(file).unwrap();
        assert!(program.entry_flow("start").is_some());
        assert!(
            program
                .source_dir(&program.entry_flow("start").unwrap())
                .is_none()
        );
        let file = atman_rt::parse_file("use \"./other.at\" as other\nflow start() {}").unwrap();
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
