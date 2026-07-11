//! Mount snapshot export for the canonical object store.
//!
//! Snapshots are a read-only replica view: canonical object bytes are rendered
//! back to their current view paths, and `index.json` records the logical id,
//! path, and blake3 for each rendered file.

use crate::cache::{Caches, object};
use crate::object_id::ObjectId;
use anyhow::{Context as _, Result, bail};
use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{Cursor, Write};
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

const INDEX_FILE: &str = "index.json";
const INDEX_VERSION: u8 = 1;

/// A read-only snapshot of one mount's canonical object bytes.
#[derive(Debug, Clone)]
pub struct MountSnapshot {
    index: SnapshotIndex,
    files: Vec<SnapshotFile>,
}

/// `index.json` written alongside snapshot files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIndex {
    pub version: u8,
    pub mount: String,
    pub files: Vec<SnapshotIndexEntry>,
}

/// One rendered canonical file recorded in `index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIndexEntry {
    pub logical_id: SnapshotLogicalId,
    pub path: String,
    pub blake3: String,
    pub size: u64,
}

/// Stable JSON form of a provider logical id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotLogicalId {
    pub kind: String,
    pub captures: Vec<SnapshotCapture>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCapture {
    pub name: String,
    pub value: String,
}

/// Cumulative progress after one file in a snapshot directory has been
/// written. The generated `index.json` is included in both totals because it
/// is part of the exported snapshot tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteProgress {
    pub files_written: u64,
    pub total_files: u64,
    pub bytes_written: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone)]
struct SnapshotFile {
    logical_id: SnapshotLogicalId,
    path: Path,
    blake3: String,
    bytes: Arc<[u8]>,
}

impl MountSnapshot {
    /// Read a mount snapshot directly from `<cache_dir>/object`.
    ///
    /// This is the CLI fallback when no compatible daemon is running. It opens
    /// the durable object database only after confirming it already exists, so
    /// a snapshot command does not create cache state for an empty workspace.
    pub fn from_cache_dir(cache_dir: &StdPath, mount: &str) -> Result<Self> {
        let object_dir = cache_dir.join("object");
        if !object_dir.exists() {
            bail!(
                "no canonical object cache at {}; run the mount before taking a snapshot",
                object_dir.display()
            );
        }
        let cache = object::Cache::open(&object_dir)
            .with_context(|| format!("open object cache {}", object_dir.display()))?;
        let objects = cache
            .mount(mount)
            .with_context(|| format!("open object cache for mount `{mount}`"))?;
        Self::from_mount_objects(mount, &objects)
    }

    pub(crate) fn from_caches(caches: &Caches, mount: &str) -> Result<Self> {
        let objects = caches
            .object
            .mount(mount)
            .with_context(|| format!("open object cache for mount `{mount}`"))?;
        Self::from_mount_objects(mount, &objects)
    }

    fn from_mount_objects(mount: &str, objects: &object::MountObjects) -> Result<Self> {
        let files = SnapshotFiles::from_entries(objects.canonical_entries()?)?.files;
        let index = SnapshotIndex::new(mount.to_string(), &files);
        Ok(Self { index, files })
    }

    /// The manifest that will be written as `index.json`.
    pub fn index(&self) -> &SnapshotIndex {
        &self.index
    }

    /// Write this snapshot as a plain directory tree.
    pub fn write_directory(&self, out: &StdPath) -> Result<()> {
        self.write_directory_with_progress(out, |_| {})
    }

    /// Write this snapshot as a plain directory tree, reporting cumulative
    /// file and byte counts after each completed file. Snapshot path mapping,
    /// index generation, and output-directory validation remain owned here;
    /// the callback is observational and cannot alter the export.
    pub fn write_directory_with_progress(
        &self,
        out: &StdPath,
        mut report: impl FnMut(WriteProgress),
    ) -> Result<()> {
        prepare_output_dir(out)?;

        let index_json = self.index_json()?;
        let total_files = u64::try_from(self.files.len())?
            .checked_add(1)
            .context("snapshot contains too many files")?;
        let total_bytes =
            self.files
                .iter()
                .try_fold(u64::try_from(index_json.len())?, |total, file| {
                    total
                        .checked_add(u64::try_from(file.bytes.len())?)
                        .context("snapshot byte count overflow")
                })?;
        let mut progress = WriteProgress {
            files_written: 0,
            total_files,
            bytes_written: 0,
            total_bytes,
        };

        for file in &self.files {
            let path = out.join(file.relative_path());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create snapshot directory {}", parent.display()))?;
            }
            std::fs::write(&path, file.bytes.as_ref())
                .with_context(|| format!("write snapshot file {}", path.display()))?;
            progress.files_written += 1;
            progress.bytes_written += u64::try_from(file.bytes.len())?;
            report(progress);
        }

        let index_path = out.join(INDEX_FILE);
        std::fs::write(&index_path, &index_json)
            .with_context(|| format!("write snapshot index {}", index_path.display()))?;
        progress.files_written += 1;
        progress.bytes_written += u64::try_from(index_json.len())?;
        report(progress);
        Ok(())
    }

    /// Write this snapshot as a tar stream.
    pub fn write_tar(&self, writer: impl Write) -> Result<()> {
        let mut tar = tar::Builder::new(writer);
        tar.mode(tar::HeaderMode::Deterministic);

        for file in &self.files {
            append_tar_file(&mut tar, &file.relative_path(), file.bytes.as_ref())?;
        }
        append_tar_file(&mut tar, StdPath::new(INDEX_FILE), &self.index_json()?)?;
        tar.finish().context("finish snapshot tar")?;
        Ok(())
    }

    /// Return this snapshot as a tar byte vector.
    pub fn to_tar_vec(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.write_tar(&mut bytes)?;
        Ok(bytes)
    }

    fn index_json(&self) -> Result<Vec<u8>> {
        let mut json = serde_json::to_vec_pretty(&self.index)?;
        json.push(b'\n');
        Ok(json)
    }
}

impl SnapshotIndex {
    fn new(mount: String, files: &[SnapshotFile]) -> Self {
        Self {
            version: INDEX_VERSION,
            mount,
            files: files.iter().map(SnapshotIndexEntry::from_file).collect(),
        }
    }
}

impl SnapshotIndexEntry {
    fn from_file(file: &SnapshotFile) -> Self {
        Self {
            logical_id: file.logical_id.clone(),
            path: file.path.as_str().to_string(),
            blake3: file.blake3.clone(),
            size: u64::try_from(file.bytes.len()).unwrap_or(u64::MAX),
        }
    }
}

impl SnapshotLogicalId {
    fn from_wit(id: wit_types::LogicalId) -> Self {
        Self {
            kind: id.kind,
            captures: id
                .captures
                .into_iter()
                .map(|capture| SnapshotCapture {
                    name: capture.name,
                    value: capture.value,
                })
                .collect(),
        }
    }
}

impl SnapshotFile {
    fn relative_path(&self) -> PathBuf {
        self.path.segments().collect()
    }
}

struct SnapshotFiles {
    files: Vec<SnapshotFile>,
}

impl SnapshotFiles {
    fn from_entries(entries: Vec<object::CanonicalEntry>) -> Result<Self> {
        let mut files = Vec::new();
        let mut paths = BTreeSet::new();

        for entry in entries {
            let object_id = ObjectId::from_bytes(entry.id);
            let Some(wit_id) = object_id.to_wit() else {
                bail!(
                    "object cache row has an undecodable logical id: {}",
                    hex::encode(object_id.as_bytes())
                );
            };
            let logical_id = SnapshotLogicalId::from_wit(wit_id);
            let bytes: Arc<[u8]> = entry.canonical.bytes.into();
            let blake3 = blake3::hash(bytes.as_ref()).to_hex().to_string();

            for leaf in entry.leaves {
                let path = Path::parse(&leaf)
                    .with_context(|| format!("object cache row has invalid leaf path `{leaf}`"))?;
                if path.is_root() {
                    bail!("object cache row maps canonical bytes to root path");
                }
                if !paths.insert(path.clone()) {
                    bail!("object cache contains duplicate canonical path `{path}`");
                }
                files.push(SnapshotFile {
                    logical_id: logical_id.clone(),
                    path,
                    blake3: blake3.clone(),
                    bytes: Arc::clone(&bytes),
                });
            }
        }

        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self { files })
    }
}

fn prepare_output_dir(out: &StdPath) -> Result<()> {
    if out.exists() {
        if !out.is_dir() {
            bail!("snapshot output {} is not a directory", out.display());
        }
        let has_entries = std::fs::read_dir(out)
            .with_context(|| format!("read snapshot output directory {}", out.display()))?
            .next()
            .transpose()?
            .is_some();
        if has_entries {
            bail!(
                "snapshot output directory {} must be empty or absent",
                out.display()
            );
        }
    } else {
        std::fs::create_dir_all(out)
            .with_context(|| format!("create snapshot output directory {}", out.display()))?;
    }
    Ok(())
}

fn append_tar_file<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &StdPath,
    bytes: &[u8],
) -> Result<()> {
    let size = u64::try_from(bytes.len()).context("snapshot file too large")?;
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, path, Cursor::new(bytes))
        .with_context(|| format!("append snapshot tar entry {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::object::{Cache, StoredObject};
    use omnifs_wit::provider::types::{IdCapture, LogicalId};
    use std::collections::BTreeMap;

    fn logical_id(name: &str) -> LogicalId {
        LogicalId {
            kind: "fixture.document".to_string(),
            captures: vec![IdCapture {
                name: "name".to_string(),
                value: name.to_string(),
            }],
        }
    }

    fn seed(cache_dir: &StdPath, leaf: &str, name: &str, bytes: &[u8]) {
        let cache = Cache::open(&cache_dir.join("object")).unwrap();
        let mount = cache.mount("fixture").unwrap();
        let id = ObjectId::from_wit(&logical_id(name));
        mount.store(
            id.as_bytes(),
            StoredObject {
                bytes: bytes.to_vec(),
                validator: None,
            },
            &[leaf.to_string()],
            |_| {},
        );
    }

    fn snapshot(cache_dir: &StdPath, out: &StdPath) -> MountSnapshot {
        let snapshot = MountSnapshot::from_cache_dir(cache_dir, "fixture").unwrap();
        snapshot.write_directory(out).unwrap();
        snapshot
    }

    fn changed_rendered_files(left: &StdPath, right: &StdPath) -> Vec<String> {
        let left_files = rendered_files(left);
        let right_files = rendered_files(right);
        left_files
            .keys()
            .chain(right_files.keys())
            .filter(|relative| relative.as_str() != INDEX_FILE)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|relative| left_files.get(*relative) != right_files.get(*relative))
            .cloned()
            .collect()
    }

    fn rendered_files(root: &StdPath) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        collect_files(root, root, &mut files);
        files
    }

    fn collect_files(root: &StdPath, dir: &StdPath, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                collect_files(root, &path, files);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(relative, std::fs::read(path).unwrap());
        }
    }

    #[test]
    fn direct_cache_snapshot_tracks_one_changed_canonical_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        seed(&cache_dir, "/docs/one.txt", "one", b"one v1\n");
        seed(&cache_dir, "/docs/two.txt", "two", b"two\n");

        let before = tmp.path().join("before");
        let before_snapshot = snapshot(&cache_dir, &before);

        seed(&cache_dir, "/docs/one.txt", "one", b"one v2\n");
        let after = tmp.path().join("after");
        let after_snapshot = snapshot(&cache_dir, &after);

        assert_eq!(
            changed_rendered_files(&before, &after),
            vec!["docs/one.txt"]
        );

        let before_hash = before_snapshot
            .index()
            .files
            .iter()
            .find(|entry| entry.path == "/docs/one.txt")
            .unwrap()
            .blake3
            .clone();
        let after_hash = after_snapshot
            .index()
            .files
            .iter()
            .find(|entry| entry.path == "/docs/one.txt")
            .unwrap()
            .blake3
            .clone();
        assert_ne!(before_hash, after_hash);
    }

    #[test]
    fn directory_write_reports_files_and_bytes_including_index() {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path().join("cache");
        let out = temp.path().join("out");
        seed(&cache_dir, "/documents/one.json", "one", b"first");
        seed(&cache_dir, "/documents/two.json", "two", b"second");
        let snapshot = MountSnapshot::from_cache_dir(&cache_dir, "fixture").unwrap();
        let mut updates = Vec::new();

        snapshot
            .write_directory_with_progress(&out, |progress| updates.push(progress))
            .unwrap();

        assert_eq!(updates.len(), 3);
        let final_update = updates.last().unwrap();
        assert_eq!(final_update.files_written, 3);
        assert_eq!(final_update.total_files, 3);
        assert_eq!(final_update.bytes_written, final_update.total_bytes);
        let bytes_on_disk = std::fs::read(out.join("documents/one.json")).unwrap().len()
            + std::fs::read(out.join("documents/two.json")).unwrap().len()
            + std::fs::read(out.join(INDEX_FILE)).unwrap().len();
        assert_eq!(final_update.total_bytes, bytes_on_disk as u64);
    }
}
