//! `ServingContext`: the mount-resolution backing a [`Tree`](crate::Tree)
//! serves.
//!
//! A `ServingContext` answers the renderer-neutral mount questions: which
//! runtime serves a mount, how a full protocol path splits into a (mount,
//! mount-relative path) pair, and (for a registry with no root-mounted
//! provider) what the synthetic mount-enumeration root lists. It wraps either
//! the production `MountRuntimes` registry both renderers hold, or a single
//! bare `Arc<Runtime>` under a fixed mount name (the kernel-free itest /
//! single-mount embedding form, because `MountRuntimes::add_mount` instantiates
//! wasm itself and cannot be populated from a bare `Runtime`).
//!
//! # Carries no policy and no scope claims
//!
//! A `ServingContext` is pure mount plumbing: it holds NO access policy and
//! makes NO scope claims. Scope enforcement must land on EVERY serving path
//! before anything named Worldview ships; until that graduation rule is met,
//! this type deliberately stays policy-free so no serving path can smuggle in a
//! half-enforced scope check. Scope belongs here only as part of that
//! graduation, applied uniformly, never as an incremental side effect.

use std::sync::Arc;

use omnifs_core::path::Path;

use crate::Runtime;
use crate::registry::MountRuntimes;
use crate::tree::error::{Result, TreeError};

const MOUNT_ENUMERATION_MOUNT: &str = "";

/// Internal mount-resolution backing. [`ServingContext::from_runtimes`] wraps a
/// full `MountRuntimes` (the production form both renderers hold);
/// [`ServingContext::single`] wraps a single bare `Arc<Runtime>` under a fixed
/// mount name (the itest / single-mount embedding form), because
/// `MountRuntimes::add_mount` instantiates wasm itself and cannot be populated
/// from a bare `Runtime`.
enum Backing {
    Registry(Arc<MountRuntimes>),
    Single {
        mount: String,
        runtime: Arc<Runtime>,
    },
}

/// The mount-resolution context a [`Tree`](crate::Tree) serves. See the module
/// docs for the no-policy / no-scope invariant and its Worldview graduation
/// rule.
pub struct ServingContext {
    backing: Backing,
}

impl ServingContext {
    /// Production form: the registry both renderers already hold.
    pub fn from_runtimes(registry: Arc<MountRuntimes>) -> Self {
        Self {
            backing: Backing::Registry(registry),
        }
    }

    /// Test/shim form for the kernel-free itest and any single-mount embedding.
    /// Wraps a bare `Arc<Runtime>` under a single mount name so a `Tree` is
    /// drivable without building a full `MountRuntimes`.
    pub fn single(mount: String, runtime: Arc<Runtime>) -> Self {
        Self {
            backing: Backing::Single { mount, runtime },
        }
    }

    /// The runtime serving `mount`, or an error if no such mount exists.
    pub(crate) fn runtime_for(&self, mount: &str) -> Result<Arc<Runtime>> {
        match &self.backing {
            Backing::Single { mount: m, runtime } if m == mount => Ok(Arc::clone(runtime)),
            Backing::Single { mount: m, .. } => Err(TreeError::not_found(format!(
                "no such mount: {mount} (single-mount tree serves {m})"
            ))),
            Backing::Registry(registry) => registry
                .get(mount)
                .ok_or_else(|| TreeError::not_found(format!("no such mount: {mount}"))),
        }
    }

    /// The runtime serving `mount` if present, without erroring. Used by the
    /// sync invalidation drain, which must no-op on an unknown mount.
    pub(crate) fn registry_runtime(&self, mount: &str) -> Option<Arc<Runtime>> {
        match &self.backing {
            Backing::Single { mount: m, runtime } if m == mount => Some(Arc::clone(runtime)),
            Backing::Single { .. } => None,
            Backing::Registry(registry) => registry.get(mount),
        }
    }

    /// Split a full protocol path into (`mount_name`, mount-relative path).
    ///
    /// For a single-mount tree the mount is fixed and the whole input path is
    /// mount-relative (the itest drives mount-relative paths like "/" and
    /// "/hello"). For a registry-backed tree the mount is the first path
    /// segment; the remainder (with a leading slash) is mount-relative. The
    /// synthetic mount-enumeration root (a bare "/" against a registry) is
    /// designed here but only the single-mount arm is exercised in slice 1.
    pub(crate) fn split_mount_path(&self, path: &Path) -> Result<(String, Path)> {
        match &self.backing {
            Backing::Single { mount, .. } => Ok((mount.clone(), path.clone())),
            Backing::Registry(registry) => {
                // A root-mounted provider claims the whole namespace.
                if let Some(root) = registry.root_mount_name() {
                    return Ok((root, path.clone()));
                }
                if path.is_root() {
                    return Ok((MOUNT_ENUMERATION_MOUNT.to_string(), Path::root()));
                }
                let mut segments = path.segments();
                let Some(mount) = segments.next() else {
                    return Err(TreeError::invalid_input(format!(
                        "split_mount_path: empty path: {}",
                        path.as_str()
                    )));
                };
                let mount = mount.to_string();
                if !registry.mounts().iter().any(|m| m == &mount) {
                    return Err(TreeError::not_found(format!("no such mount: {mount}")));
                }
                let rest = path
                    .as_str()
                    .strip_prefix(&format!("/{mount}"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or("/");
                let rel = Path::parse(rest).map_err(|e| {
                    TreeError::invalid_input(format!("invalid mount-relative path: {e}"))
                })?;
                Ok((mount, rel))
            },
        }
    }

    pub(crate) fn is_mount_enumeration_root(&self, mount: &str, path: &Path) -> bool {
        matches!(&self.backing, Backing::Registry(registry) if registry.root_mount_name().is_none())
            && mount == MOUNT_ENUMERATION_MOUNT
            && path.is_root()
    }

    pub(crate) fn mount_names(&self) -> Option<Vec<String>> {
        match &self.backing {
            Backing::Registry(registry) if registry.root_mount_name().is_none() => {
                let mut mounts = registry.mounts();
                mounts.sort();
                Some(mounts)
            },
            Backing::Registry(_) | Backing::Single { .. } => None,
        }
    }
}
