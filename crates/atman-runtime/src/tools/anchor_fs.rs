//! Hashline anchors and durable snapshot/change state.
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, Error as SqlError, ErrorCode, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const HASH_ALGORITHM: &str = "anchor-v1";
const BASE: u32 = 62;
const ANCHOR_SPACE: u32 = BASE * BASE * BASE;
const PROBE_STRIDE: u32 = BASE * BASE + BASE + 1;
const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

#[derive(Debug, Error)]
pub enum AnchorError {
    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("SQLite error for {path}: {source}")]
    Sql {
        path: PathBuf,
        #[source]
        source: SqlError,
    },
    #[error("invalid UTF-8 in {path}")]
    InvalidUtf8 { path: PathBuf },
    #[error("missing file: {0}")]
    MissingFile(PathBuf),
    #[error("corrupt state at {path}: {reason}")]
    CorruptState { path: PathBuf, reason: String },
    #[error("strict undo refused for {path}: current hash is {actual}, expected {expected}")]
    UndoConflict {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("strict anchor resolution failed for {path}: {reason}")]
    Resolve { path: PathBuf, reason: String },
    #[error("change not found: {0}")]
    ChangeNotFound(String),
    #[error("file hash conflict for {path}: current hash is {actual}, expected {expected}")]
    HashConflict {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("atomic replace is unsupported on Windows without ReplaceFileW: {0}")]
    UnsupportedAtomicReplace(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnchorLine {
    pub anchor: String,
    pub hash: String,
    pub line: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub path: String,
    pub checksum: String,
    pub line_count: usize,
    pub hashes: Vec<String>,
    pub hash_algorithm: String,
    pub updated_at: u64,
    pub anchors: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeRecord {
    pub change_id: String,
    pub path: String,
    pub before_hash: String,
    pub after_hash: String,
    pub before_content: String,
    pub after_content: String,
    pub parent_change_id: Option<String>,
    pub created_at: u64,
}

pub struct StateStore {
    root: PathBuf,
}
impl StateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn db_path(&self) -> PathBuf {
        self.root.join("anchor-state.sqlite3")
    }
    fn connect(&self) -> Result<Connection, AnchorError> {
        fs::create_dir_all(&self.root).map_err(|e| io_error(&self.root, e))?;
        let path = self.db_path();
        match self.open_connection(&path).and_then(|c| {
            init_schema(&c, &path)?;
            Ok(c)
        }) {
            Ok(c) => Ok(c),
            Err(first) if path.exists() => {
                let corrupt = self
                    .root
                    .join(format!("anchor-state.sqlite3.corrupt-{}", now()));
                let _ = fs::rename(&path, &corrupt);
                let _ = fs::remove_file(format!("{}-wal", path.display()));
                let _ = fs::remove_file(format!("{}-shm", path.display()));
                self.open_connection(&path)
                    .and_then(|c| {
                        init_schema(&c, &path)?;
                        Ok(c)
                    })
                    .map_err(|_| first)
            }
            Err(e) => Err(e),
        }
    }
    fn open_connection(&self, path: &Path) -> Result<Connection, AnchorError> {
        let c = retry_busy(|| Connection::open(path), path)?;
        retry_busy(|| c.busy_timeout(Duration::from_millis(2500)), path)?;
        retry_busy(|| c.pragma_update(None, "journal_mode", "WAL"), path)?;
        retry_busy(|| c.pragma_update(None, "synchronous", "NORMAL"), path)?;
        Ok(c)
    }
    pub fn snapshot(&self, path: &Path) -> Result<Option<Snapshot>, AnchorError> {
        let c = self.connect()?;
        let mut q = c
            .prepare("SELECT data FROM snapshots WHERE path=?1")
            .map_err(|e| sql_error(&self.db_path(), e))?;
        let value = q
            .query_row(params![path.to_string_lossy()], |r| r.get::<_, Vec<u8>>(0))
            .optional()
            .map_err(|e| sql_error(&self.db_path(), e))?;
        value
            .map(|b| serde_json::from_slice(&b).map_err(|e| corrupt(&self.db_path(), e)))
            .transpose()
    }
    pub fn put_snapshot(&self, snapshot: Snapshot) -> Result<(), AnchorError> {
        validate_snapshot(&snapshot)?;
        let data = serde_json::to_vec(&snapshot).map_err(|e| corrupt(&self.db_path(), e))?;
        let c = self.connect()?;
        retry_busy(|| c.execute("INSERT INTO snapshots(path,data) VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET data=excluded.data", params![snapshot.path, data]), &self.db_path()).map(|_| ())
    }
    pub fn record_change(&self, change: ChangeRecord) -> Result<(), AnchorError> {
        let data = serde_json::to_vec(&change).map_err(|e| corrupt(&self.db_path(), e))?;
        let c = self.connect()?;
        retry_busy(|| c.execute("INSERT OR REPLACE INTO changes(change_id,path,created_at,data) VALUES(?1,?2,?3,?4)", params![change.change_id, change.path, change.created_at, data]), &self.db_path()).map(|_| ())
    }
    pub fn latest_change(&self, path: &Path) -> Result<Option<ChangeRecord>, AnchorError> {
        let c = self.connect()?;
        let data = c.query_row("SELECT data FROM changes WHERE path=?1 ORDER BY created_at DESC, change_id DESC LIMIT 1", params![path.to_string_lossy()], |r| r.get::<_, Vec<u8>>(0)).optional().map_err(|e| sql_error(&self.db_path(), e))?;
        data.map(|b| serde_json::from_slice(&b).map_err(|e| corrupt(&self.db_path(), e)))
            .transpose()
    }
    pub fn change(&self, change_id: &str) -> Result<Option<ChangeRecord>, AnchorError> {
        let c = self.connect()?;
        let data = c
            .query_row(
                "SELECT data FROM changes WHERE change_id=?1",
                params![change_id],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|e| sql_error(&self.db_path(), e))?;
        data.map(|b| serde_json::from_slice(&b).map_err(|e| corrupt(&self.db_path(), e)))
            .transpose()
    }
    pub fn undo_strict(
        &self,
        path: &Path,
        change: &ChangeRecord,
        current: &[u8],
    ) -> Result<(), AnchorError> {
        let actual = file_hash(current);
        if actual != change.after_hash {
            return Err(AnchorError::UndoConflict {
                path: path.to_path_buf(),
                expected: change.after_hash.clone(),
                actual,
            });
        }
        atomic_replace(path, change.before_content.as_bytes())
    }
}

fn init_schema(c: &Connection, path: &Path) -> Result<(), AnchorError> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS snapshots(path TEXT PRIMARY KEY, data BLOB NOT NULL); CREATE TABLE IF NOT EXISTS changes(change_id TEXT PRIMARY KEY, path TEXT NOT NULL, created_at INTEGER NOT NULL, data BLOB NOT NULL); CREATE INDEX IF NOT EXISTS changes_path_created ON changes(path, created_at);").map_err(|e| sql_error(path, e))
}
fn retry_busy<T, F: FnMut() -> Result<T, SqlError>>(
    mut f: F,
    path: &Path,
) -> Result<T, AnchorError> {
    for attempt in 0..6 {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if is_busy(&e) && attempt < 5 => {
                thread::sleep(Duration::from_millis(10 * (attempt + 1)))
            }
            Err(e) => return Err(sql_error(path, e)),
        }
    }
    unreachable!()
}
fn is_busy(e: &SqlError) -> bool {
    matches!(e, SqlError::SqliteFailure(x, _) if matches!(x.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}
fn corrupt(path: &Path, e: impl std::fmt::Display) -> AnchorError {
    AnchorError::CorruptState {
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}
fn sql_error(path: &Path, e: SqlError) -> AnchorError {
    AnchorError::Sql {
        path: path.to_path_buf(),
        source: e,
    }
}
fn io_error(path: &Path, source: io::Error) -> AnchorError {
    AnchorError::Io {
        path: path.to_path_buf(),
        source,
    }
}
fn validate_snapshot(s: &Snapshot) -> Result<(), AnchorError> {
    if s.hash_algorithm != HASH_ALGORITHM
        || s.hashes.len() != s.line_count
        || s.anchors.len() != s.line_count
        || s.anchors
            .iter()
            .any(|a| a.len() != 3 || !a.bytes().all(|b| ALPHABET.contains(&b)))
    {
        return Err(corrupt(Path::new(&s.path), "invalid snapshot"));
    }
    Ok(())
}

pub fn file_hash(content: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(content);
    format!("sha256:{:x}", h.finalize())
}
pub fn canonical_line(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line).trim_end()
}
pub fn anchor_for_hash(hash: u32) -> String {
    let mut v = hash % ANCHOR_SPACE;
    let mut out = [b'A'; 3];
    for i in (0..3).rev() {
        out[i] = ALPHABET[(v % BASE) as usize];
        v /= BASE;
    }
    String::from_utf8(out.to_vec()).unwrap()
}
pub fn line_hash(line: &str) -> u32 {
    xxh32(canonical_line(line).as_bytes(), 0) >> 14
}
pub fn anchors_for_lines(
    lines: &[String],
    previous: Option<&Snapshot>,
) -> Result<Vec<String>, AnchorError> {
    let mut used = HashSet::new();
    let mut result = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let hash = format!("{:08x}", line_hash(line));
        let mut value = previous
            .and_then(|s| (s.hashes.get(i) == Some(&hash)).then(|| s.anchors[i].clone()))
            .filter(|a| used.insert(a.clone()));
        if value.is_none() {
            let mut slot = line_hash(line) % ANCHOR_SPACE;
            for _ in 0..ANCHOR_SPACE {
                let a = anchor_for_hash(slot);
                if used.insert(a.clone()) {
                    value = Some(a);
                    break;
                }
                slot = (slot + PROBE_STRIDE) % ANCHOR_SPACE;
            }
        }
        result.push(value.ok_or_else(|| corrupt(Path::new("anchors"), "anchor space exhausted"))?);
    }
    Ok(result)
}
pub fn make_snapshot(
    path: &Path,
    content: &str,
    previous: Option<&Snapshot>,
) -> Result<Snapshot, AnchorError> {
    let lines = split_lines(content);
    let hashes = lines
        .iter()
        .map(|l| format!("{:08x}", line_hash(l)))
        .collect();
    let anchors = anchors_for_lines(&lines, previous)?;
    let checksum = file_hash(content.as_bytes());
    Ok(Snapshot {
        snapshot_id: format!("snap_{}", &checksum[7..23]),
        path: path.to_string_lossy().into(),
        checksum,
        line_count: lines.len(),
        hashes,
        hash_algorithm: HASH_ALGORITHM.into(),
        updated_at: now(),
        anchors,
    })
}
pub fn split_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split_inclusive('\n').map(str::to_owned).collect()
    }
}

/// Render the current file as stable hashline records.
pub fn read_anchor_text(path: &Path, store: &StateStore) -> Result<String, AnchorError> {
    let content = read_text(path)?;
    let previous = store.snapshot(path)?;
    let snapshot = make_snapshot(path, &content, previous.as_ref())?;
    let lines = split_lines(&content);
    let output = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            format!(
                "{}:{}│{}",
                i + 1,
                snapshot.anchors[i],
                line.trim_end_matches(['\r', '\n'])
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    store.put_snapshot(snapshot)?;
    Ok(output)
}

/// The supported line-oriented mutation operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOperation {
    Replace,
    Insert,
    Remove,
}

impl MutationOperation {
    fn parse(value: &str) -> Result<Self, AnchorError> {
        match value {
            "replace" => Ok(Self::Replace),
            "insert" => Ok(Self::Insert),
            "remove" => Ok(Self::Remove),
            _ => Err(AnchorError::Resolve {
                path: PathBuf::new(),
                reason: format!("unknown operation {value}"),
            }),
        }
    }
}

fn read_text(path: &Path) -> Result<String, AnchorError> {
    let bytes = fs::read(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            AnchorError::MissingFile(path.to_path_buf())
        } else {
            io_error(path, e)
        }
    })?;
    String::from_utf8(bytes).map_err(|_| AnchorError::InvalidUtf8 {
        path: path.to_path_buf(),
    })
}

fn resolve_endpoint(
    path: &Path,
    snapshot: &Snapshot,
    lines: &[String],
    endpoint: &str,
) -> Result<usize, AnchorError> {
    if endpoint.len() != 3 || !endpoint.bytes().all(|byte| ALPHABET.contains(&byte)) {
        return Err(AnchorError::Resolve {
            path: path.to_path_buf(),
            reason: format!("invalid endpoint {endpoint}; expected a 3-character anchor"),
        });
    }
    let anchor = endpoint;
    let matches: Vec<_> = snapshot
        .anchors
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.as_str() == anchor)
        .map(|(index, _)| index)
        .collect();
    let raw_count = lines
        .iter()
        .filter(|line| anchor_for_hash(line_hash(line)) == anchor)
        .count();
    if raw_count > 1 {
        return Err(AnchorError::Resolve {
            path: path.to_path_buf(),
            reason: format!("ambiguous endpoint {endpoint}"),
        });
    }
    if matches.len() != 1 {
        return Err(AnchorError::Resolve {
            path: path.to_path_buf(),
            reason: format!("stale or missing endpoint {endpoint}"),
        });
    }
    Ok(matches[0])
}

/// Apply a strict anchor mutation and persist its inverse in the WAL.
#[allow(clippy::too_many_arguments)]
pub fn edit_by_anchor(
    path: &Path,
    operation: &str,
    target: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    at: Option<&str>,
    position: Option<&str>,
    content: Option<&str>,
    store: &StateStore,
) -> Result<ChangeRecord, AnchorError> {
    let op = MutationOperation::parse(operation).map_err(|e| AnchorError::Resolve {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let before = read_text(path)?;
    let old_hash = file_hash(before.as_bytes());
    let old_snapshot = store.snapshot(path)?;
    let current_snapshot = make_snapshot(path, &before, old_snapshot.as_ref())?;
    let old_lines = split_lines(&before);
    let (start, end) = match op {
        MutationOperation::Replace | MutationOperation::Remove => {
            if let Some(t) = target {
                let i = resolve_endpoint(path, &current_snapshot, &old_lines, t)?;
                (i, i)
            } else {
                (
                    resolve_endpoint(
                        path,
                        &current_snapshot,
                        &old_lines,
                        from.ok_or_else(|| AnchorError::Resolve {
                            path: path.to_path_buf(),
                            reason: "from is required".into(),
                        })?,
                    )?,
                    resolve_endpoint(
                        path,
                        &current_snapshot,
                        &old_lines,
                        to.ok_or_else(|| AnchorError::Resolve {
                            path: path.to_path_buf(),
                            reason: "to is required".into(),
                        })?,
                    )?,
                )
            }
        }
        MutationOperation::Insert => {
            let e = at.or(target).or(from).ok_or_else(|| AnchorError::Resolve {
                path: path.to_path_buf(),
                reason: "insert endpoint is required".into(),
            })?;
            let i = resolve_endpoint(path, &current_snapshot, &old_lines, e)?;
            if position == Some("after") {
                (i + 1, i + 1)
            } else {
                (i, i)
            }
        }
    };
    if start > end || end > old_lines.len() {
        return Err(AnchorError::Resolve {
            path: path.to_path_buf(),
            reason: "range endpoints are reversed".into(),
        });
    }
    let inserted = content.unwrap_or("");
    let mut new_lines = old_lines[..start].to_vec();
    if op != MutationOperation::Remove {
        new_lines.extend(split_lines(inserted));
    }
    if op == MutationOperation::Insert {
        new_lines.extend(old_lines[start..].iter().cloned());
    } else {
        new_lines.extend(old_lines[end + 1..].iter().cloned());
    }
    let after = new_lines.concat();
    let new_hash = file_hash(after.as_bytes());
    atomic_replace(path, after.as_bytes())?;
    let snapshot = make_snapshot(path, &after, old_snapshot.as_ref())?;
    store.put_snapshot(snapshot)?;
    let change = ChangeRecord {
        change_id: format!("change_{}_{}", now(), &new_hash[7..19]),
        path: path.to_string_lossy().into(),
        before_hash: old_hash,
        after_hash: new_hash,
        before_content: before,
        after_content: after,
        parent_change_id: store.latest_change(path)?.map(|c| c.change_id),
        created_at: now(),
    };
    store.record_change(change.clone())?;
    Ok(change)
}

pub fn overwrite_with_hash(
    path: &Path,
    expected_file_hash: &str,
    content: &str,
    store: &StateStore,
) -> Result<ChangeRecord, AnchorError> {
    let before = read_text(path)?;
    let actual = file_hash(before.as_bytes());
    if actual != expected_file_hash {
        return Err(AnchorError::HashConflict {
            path: path.to_path_buf(),
            expected: expected_file_hash.into(),
            actual,
        });
    }
    let previous = store.snapshot(path)?;
    atomic_replace(path, content.as_bytes())?;
    let snapshot = make_snapshot(path, content, previous.as_ref())?;
    store.put_snapshot(snapshot)?;
    let after_hash = file_hash(content.as_bytes());
    let change = ChangeRecord {
        change_id: format!("change_{}_{}", now(), &after_hash[7..19]),
        path: path.to_string_lossy().into(),
        before_hash: actual,
        after_hash,
        before_content: before,
        after_content: content.into(),
        parent_change_id: store.latest_change(path)?.map(|c| c.change_id),
        created_at: now(),
    };
    store.record_change(change.clone())?;
    Ok(change)
}

pub fn undo_change(change_id: &str, store: &StateStore) -> Result<(), AnchorError> {
    let change = store
        .change(change_id)?
        .ok_or_else(|| AnchorError::ChangeNotFound(change_id.into()))?;
    let path = Path::new(&change.path);
    let current = fs::read(path).map_err(|e| io_error(path, e))?;
    store.undo_strict(path, &change, &current)?;
    let previous = store.snapshot(path)?;
    let restored = make_snapshot(path, &change.before_content, previous.as_ref())?;
    store.put_snapshot(restored)
}

pub fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), AnchorError> {
    #[cfg(windows)]
    {
        let _ = bytes;
        return Err(AnchorError::UnsupportedAtomicReplace(path.to_path_buf()));
    }
    #[cfg(unix)]
    {
        atomic_replace_unix(path, bytes)
    }
}
#[cfg(unix)]
fn atomic_replace_unix(path: &Path, bytes: &[u8]) -> Result<(), AnchorError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
    let tmp = parent.join(format!(
        ".{}.anchor-tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        now()
    ));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| io_error(&tmp, e))?;
        f.write_all(bytes).map_err(|e| io_error(&tmp, e))?;
        f.sync_all().map_err(|e| io_error(&tmp, e))?;
        fs::rename(&tmp, path).map_err(|e| io_error(path, e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
pub fn read_utf8(path: &Path) -> Result<String, AnchorError> {
    let bytes = fs::read(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            AnchorError::MissingFile(path.to_path_buf())
        } else {
            io_error(path, e)
        }
    })?;
    String::from_utf8(bytes).map_err(|_| AnchorError::InvalidUtf8 {
        path: path.to_path_buf(),
    })
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn xxh32(input: &[u8], seed: u32) -> u32 {
    const P1: u32 = 0x9E3779B1;
    const P2: u32 = 0x85EBCA77;
    const P3: u32 = 0xC2B2AE3D;
    const P4: u32 = 0x27D4EB2F;
    const P5: u32 = 0x165667B1;
    fn r(x: u32, n: u32) -> u32 {
        x.rotate_left(n)
    }
    let mut h = seed.wrapping_add(P5).wrapping_add(input.len() as u32);
    let mut i = 0;
    while i + 4 <= input.len() {
        h = h
            .wrapping_add(u32::from_le_bytes(input[i..i + 4].try_into().unwrap()).wrapping_mul(P3));
        h = r(h, 17).wrapping_mul(P4);
        i += 4;
    }
    while i < input.len() {
        h = h.wrapping_add((input[i] as u32).wrapping_mul(P5));
        h = r(h, 11).wrapping_mul(P1);
        i += 1;
    }
    h ^= h >> 15;
    h = h.wrapping_mul(P2);
    h ^= h >> 13;
    h = h.wrapping_mul(P3);
    h ^ (h >> 16)
}
