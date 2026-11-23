//! Profile-configured, handle-relative discovery of host workspaces.

use std::{
    collections::BTreeMap,
    fs::{Metadata, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::flow_store::profile::ProfileStore;

mod unix;

const POLICY_BYTES: u64 = 32 * 1024;
const MAX_ROOTS: usize = 16;
const MAX_SCANNED: usize = 4096;
static DIRECTORY_READER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Debug)]
pub enum BrowseError {
    Busy,
    InvalidPolicy,
    UnknownRoot,
    Unavailable,
    Changed,
    TooLarge,
    Unsupported,
    ReadOnlyRoot,
}

type Result<T> = std::result::Result<T, BrowseError>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    roots: BTreeMap<String, Root>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    path: PathBuf,
    label: String,
    #[serde(default)]
    allow_open: bool,
}

pub struct ObservedFolders {
    pub boundary: DirectoryBoundary,
    pub names: Vec<String>,
}

/// Revision of the configured root and the actual objects opened during observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryBoundary {
    grant: DirectoryGrant,
    profile: Uuid,
    revision: blake3::Hash,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryGrant {
    policy: [u8; 32],
    trail: Vec<(u64, u64)>,
}

#[derive(Clone)]
pub struct WorkspaceSelection {
    pub root_id: String,
    pub components: Vec<String>,
    pub boundary: DirectoryBoundary,
}

/// The recorded authority for an accepted workspace operation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkspaceGrant {
    pub(crate) root_id: String,
    pub(crate) components: Vec<String>,
    directory: DirectoryGrant,
}

#[derive(Clone)]
pub struct WorkspaceReader {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl WorkspaceReader {
    pub fn new(config_dir: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            config_dir,
            data_dir,
        }
    }

    pub async fn roots(self) -> Result<Vec<(String, String)>> {
        self.read(false, |reader, _| {
            let (config, _) = reader.policy()?;
            Ok(config
                .roots
                .into_iter()
                .map(|(id, root)| (id, root.label))
                .collect())
        })
        .await
    }

    pub async fn folders(self, root: String, parts: Vec<String>) -> Result<ObservedFolders> {
        self.read(false, move |reader, cancelled| {
            reader.observe(&root, &parts, &cancelled)
        })
        .await
    }

    pub async fn check_selection(self, selection: WorkspaceSelection) -> Result<()> {
        self.read(true, move |reader, _| {
            let profile = reader.profile()?;
            let grant = reader.grant(selection, profile, false)?;
            reader.verify_grant(&grant, profile).map(drop)
        })
        .await
    }

    async fn read<T: Send + 'static>(
        self,
        wait: bool,
        work: impl FnOnce(Self, CancellationToken) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = if wait {
            DIRECTORY_READER
                .acquire()
                .await
                .map_err(|_| BrowseError::Unavailable)?
        } else {
            DIRECTORY_READER
                .try_acquire()
                .map_err(|_| BrowseError::Busy)?
        };
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone().drop_guard();
        let result = crate::read_work::capture(move || {
            let _permit = permit;
            work(self, cancellation)
        })
        .await
        .map_err(|_| BrowseError::Unavailable)?;
        drop(cancel);
        result
    }

    fn profile(&self) -> Result<Uuid> {
        match ProfileStore::inspect_identity(&self.data_dir) {
            Ok(profile) => Ok(profile),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Uuid::nil()),
            Err(_) => Err(BrowseError::InvalidPolicy),
        }
    }

    fn policy(&self) -> Result<(Config, [u8; 32])> {
        let path = self.config_dir.join("workspace-roots.toml");
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((
                    Config {
                        roots: BTreeMap::new(),
                    },
                    *blake3::hash(b"").as_bytes(),
                ));
            }
            Err(_) => return Err(BrowseError::InvalidPolicy),
        };
        let before = file.metadata().map_err(|_| BrowseError::InvalidPolicy)?;
        if !before.is_file() || before.len() > POLICY_BYTES {
            return Err(BrowseError::InvalidPolicy);
        }
        let mut bytes = String::new();
        (&mut file)
            .take(POLICY_BYTES + 1)
            .read_to_string(&mut bytes)
            .map_err(|_| BrowseError::InvalidPolicy)?;
        let after = file.metadata().map_err(|_| BrowseError::InvalidPolicy)?;
        if bytes.len() as u64 > POLICY_BYTES || changed(&before, &after) {
            return Err(BrowseError::InvalidPolicy);
        }
        let config: Config = toml::from_str(&bytes).map_err(|_| BrowseError::InvalidPolicy)?;
        if config.roots.len() > MAX_ROOTS {
            return Err(BrowseError::InvalidPolicy);
        }
        let private = self.private_paths();
        for (id, root) in &config.roots {
            if !root_id(id)
                || root.label.trim().is_empty()
                || root.label.len() > 128
                || root.label.chars().any(char::is_control)
                || !root.path.is_absolute()
                || root.path.parent().is_none()
                || root.path.as_os_str().len() > 4096
                || root.path.components().any(|c| c == Component::ParentDir)
                || private
                    .iter()
                    .any(|path| root.path.starts_with(path) || path.starts_with(&root.path))
            {
                return Err(BrowseError::InvalidPolicy);
            }
        }
        let mut revision = blake3::Hasher::new();
        stamp(&mut revision, &before);
        revision.update(bytes.as_bytes());
        Ok((config, *revision.finalize().as_bytes()))
    }

    fn observe(
        &self,
        root_id: &str,
        parts: &[String],
        cancelled: &CancellationToken,
    ) -> Result<ObservedFolders> {
        if !valid_path(parts) {
            return Err(BrowseError::Unavailable);
        }
        let profile = self.profile()?;
        let (config, policy) = self.policy()?;
        let root = config.roots.get(root_id).ok_or(BrowseError::UnknownRoot)?;
        let (directory, trail) = unix::walk(&root.path, parts).map_err(directory_error)?;
        self.check_private_identity(&trail, parts.len())?;
        let before = directory.metadata().map_err(directory_error)?;
        let mut names = unix::names(&directory, cancelled)?;
        let (reopened, current) =
            unix::walk(&root.path, parts).map_err(|_| BrowseError::Changed)?;
        if trail != current
            || changed(&before, &reopened.metadata().map_err(directory_error)?)
            || self.policy()?.1 != policy
            || self.profile()? != profile
        {
            return Err(BrowseError::Changed);
        }
        self.check_private_identity(&current, parts.len())?;
        names.sort_unstable();
        let mut revision = blake3::Hasher::new();
        stamp(&mut revision, &before);
        for name in &names {
            revision.update(name.as_bytes()).update(b"\0");
        }
        Ok(ObservedFolders {
            boundary: DirectoryBoundary {
                grant: DirectoryGrant { policy, trail },
                profile,
                revision: revision.finalize(),
            },
            names,
        })
    }

    pub(crate) fn grant(
        &self,
        selection: WorkspaceSelection,
        profile: Uuid,
        cold_profile: bool,
    ) -> Result<WorkspaceGrant> {
        if selection.boundary.profile != profile
            && !(cold_profile && selection.boundary.profile.is_nil())
        {
            return Err(BrowseError::Changed);
        }
        Ok(WorkspaceGrant {
            root_id: selection.root_id,
            components: selection.components,
            directory: selection.boundary.grant,
        })
    }

    pub(crate) fn verify_grant(&self, grant: &WorkspaceGrant, profile: Uuid) -> Result<PathBuf> {
        if !root_id(&grant.root_id) || !valid_path(&grant.components) || self.profile()? != profile
        {
            return Err(BrowseError::Changed);
        }
        let (config, policy) = self.policy()?;
        let root = config
            .roots
            .get(&grant.root_id)
            .ok_or(BrowseError::Changed)?;
        if !root.allow_open {
            return Err(BrowseError::ReadOnlyRoot);
        }
        if policy != grant.directory.policy {
            return Err(BrowseError::Changed);
        }
        let (directory, trail) =
            unix::walk_trusted(&root.path, &grant.components).map_err(directory_error)?;
        self.check_private_identity(&trail, grant.components.len())?;
        if trail != grant.directory.trail {
            return Err(BrowseError::Changed);
        }
        unix::check_project_config(&directory).map_err(directory_error)?;
        if self.policy()?.1 != policy || self.profile()? != profile {
            return Err(BrowseError::Changed);
        }
        let mut path: PathBuf = root.path.components().collect();
        path.extend(&grant.components);
        Ok(path)
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    fn private_paths(&self) -> [PathBuf; 2] {
        [&self.config_dir, &self.data_dir].map(|path| crate::fs_access::canonicalize_stable(path))
    }

    fn check_private_identity(&self, trail: &[(u64, u64)], depth: usize) -> Result<()> {
        let root = trail[trail.len() - depth - 1];
        for private in self.private_paths() {
            // Platform aliases can give an existing directory two absolute paths.
            if private
                .metadata()
                .is_ok_and(|metadata| trail.contains(&identity(&metadata)))
                || private.ancestors().skip(1).any(|path| {
                    path.metadata()
                        .is_ok_and(|metadata| identity(&metadata) == root)
                })
            {
                return Err(BrowseError::InvalidPolicy);
            }
        }
        Ok(())
    }
}

pub fn root_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

pub fn valid_path(parts: &[String]) -> bool {
    parts.len() <= 32
        && parts.iter().all(|part| valid_name(part))
        && parts.iter().map(|name| name.len() + 1).sum::<usize>() <= 4096
}

fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('.')
        || name == "~"
        || name.ends_with([' ', '.'])
        || name.chars().any(|ch| {
            ch.is_control() || matches!(ch, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

fn stamp(hasher: &mut blake3::Hasher, metadata: &Metadata) {
    for value in [
        metadata.dev(),
        metadata.ino(),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ] {
        hasher.update(&value.to_le_bytes());
    }
}

fn identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn changed(before: &Metadata, after: &Metadata) -> bool {
    (
        before.dev(),
        before.ino(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
    ) != (
        after.dev(),
        after.ino(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    )
}

fn directory_error(error: io::Error) -> BrowseError {
    if error.raw_os_error() == Some(libc::ENOSYS) {
        BrowseError::Unsupported
    } else {
        BrowseError::Unavailable
    }
}
