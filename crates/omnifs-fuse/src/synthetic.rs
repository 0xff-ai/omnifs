//! Host-synthesized paths: pagination controls and mount-root ignore files.

use fuser::{Errno, FileAttr};
use omnifs_cache::RecordKind;
use omnifs_core::view::DirentRecord;
use omnifs_host::Runtime;
use omnifs_host::pagination::{self, NextPageOutcome};
use omnifs_host::wit_protocol;
use omnifs_inspector::TraceId;
use std::time::Duration;
use tokio::runtime::Handle;

use super::Frontend;
use super::common::{DirSnapshot, is_mount_root, join_child_path};

impl Frontend {
    /// Resolve a pagination control name from the parent's cached dirents.
    pub(crate) fn lookup_synthetic_control(
        &self,
        mount_name: &str,
        parent_path: &str,
        name_str: &str,
    ) -> Result<Option<(FileAttr, Duration)>, Errno> {
        if !pagination::is_control_name(name_str) {
            return Ok(None);
        }
        let child_path = join_child_path(parent_path, name_str);
        let Some(dirent) = self.cached_control_dirent(mount_name, parent_path, name_str) else {
            return Err(Errno::ENOENT);
        };
        let ino = self.get_or_alloc_ino_meta(mount_name, &child_path, dirent.meta.clone());
        let kind = wit_protocol::entry_kind_to_wit(&dirent.meta.kind);
        Ok(Some((
            self.attr_for_inode_or_meta(ino, &kind, dirent.meta.st_size()),
            Frontend::ttl_for_meta(&dirent.meta),
        )))
    }

    /// True when path dedup must not short-circuit provider lookup.
    pub(crate) fn skip_dedup_for_root_ignore(
        entry_synthetic: bool,
        parent_path: &str,
        name_str: &str,
    ) -> bool {
        entry_synthetic && is_mount_root(parent_path) && pagination::is_ignore_name(name_str)
    }

    /// Run `@next`/`@all` and return status text for the control file read path.
    pub(crate) fn serve_synthetic_control_read(
        &self,
        rt: &Handle,
        mount_name: &str,
        parent_path: &str,
        leaf: &str,
        trace: Option<TraceId>,
    ) -> Option<String> {
        let runtime = self.runtime_for_mount(mount_name)?;
        let status = if leaf == pagination::CTRL_ALL {
            rt.block_on(runtime.paginate_all(parent_path, trace))
        } else {
            match rt.block_on(runtime.paginate_next(parent_path, trace)) {
                NextPageOutcome::Loaded { added, more } => format!(
                    "loaded +{added} entries; {}\n",
                    if more { "more available" } else { "complete" }
                ),
                NextPageOutcome::NoMore => "no more pages\n".to_string(),
                NextPageOutcome::Error(message) => message,
            }
        };
        self.mem_invalidate(mount_name, parent_path, RecordKind::Dirents);
        Some(status)
    }

    /// Append synthetic control dirents to a fresh listing snapshot.
    pub(crate) fn append_synthetic_control_entries(
        &self,
        mount_name: &str,
        path: &str,
        snapshot: &mut DirSnapshot,
        dirent_records: &mut Vec<DirentRecord>,
    ) {
        for record in Runtime::control_entries() {
            let child_path = join_child_path(path, &record.name);
            let child_ino =
                self.get_or_alloc_ino_meta(mount_name, &child_path, record.meta.clone());
            snapshot.push((
                child_ino,
                record.name.clone(),
                wit_protocol::entry_kind_to_wit(&record.meta.kind),
            ));
            dirent_records.push(record);
        }
    }
}
