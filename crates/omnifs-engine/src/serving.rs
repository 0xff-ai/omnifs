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
//! A registry-backed `ServingContext` may carry a Worldview: a named read-only
//! serving scope over the mounted namespace. The scope is enforced here because
//! every tree operation passes through mount splitting, runtime lookup, or root
//! mount enumeration before it can touch a provider.

use std::collections::BTreeMap;
use std::sync::Arc;

use omnifs_core::path::Path;
use omnifs_workspace::worldviews::Worldview;

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
#[derive(Clone)]
enum Backing {
    Registry {
        registry: Arc<MountRuntimes>,
        scope: Scope,
    },
    Single {
        mount: String,
        runtime: Arc<Runtime>,
    },
}

/// The mount-resolution context a [`Tree`](crate::Tree) serves. See the module
/// docs for the Worldview enforcement boundary.
#[derive(Clone)]
pub struct ServingContext {
    backing: Backing,
}

impl ServingContext {
    /// Production form: the registry both renderers already hold.
    pub fn from_runtimes(registry: Arc<MountRuntimes>) -> Self {
        Self {
            backing: Backing::Registry {
                registry,
                scope: Scope::All,
            },
        }
    }

    /// Production form scoped by a named Worldview. Mounts not named by the
    /// worldview do not exist through this context, and mount-relative subtree
    /// prefixes are enforced before provider dispatch.
    pub fn from_worldview(registry: Arc<MountRuntimes>, worldview: &Worldview) -> Self {
        Self {
            backing: Backing::Registry {
                registry,
                scope: Scope::from_worldview(worldview),
            },
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
    pub(crate) fn runtime_for(&self, mount: &str, target: &Path) -> Result<Arc<Runtime>> {
        match &self.backing {
            Backing::Single { mount: m, runtime } if m == mount => Ok(Arc::clone(runtime)),
            Backing::Single { mount: m, .. } => Err(TreeError::not_found(format!(
                "no such mount: {mount} (single-mount tree serves {m})"
            ))),
            Backing::Registry { registry, scope } => {
                scope.ensure_provider_path(mount, target)?;
                registry
                    .get(mount)
                    .ok_or_else(|| TreeError::not_found(format!("no such mount: {mount}")))
            },
        }
    }

    /// The runtime serving `mount` if present, without erroring. Used by the
    /// sync invalidation drain, which must no-op on an unknown mount.
    pub(crate) fn registry_runtime(&self, mount: &str) -> Option<Arc<Runtime>> {
        match &self.backing {
            Backing::Single { mount: m, runtime } if m == mount => Some(Arc::clone(runtime)),
            Backing::Single { .. } => None,
            Backing::Registry { registry, .. } => {
                // Invalidation is host bookkeeping, not serving. A Worldview
                // narrows what clients can traverse, but the host may still
                // drain invalidations from every running runtime.
                registry.get(mount)
            },
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
            Backing::Registry { registry, scope } => {
                // A root-mounted provider claims the whole namespace.
                if let Some(root) = registry.root_mount_name() {
                    if !scope.serves_mount(&root) {
                        return if path.is_root() {
                            Ok((MOUNT_ENUMERATION_MOUNT.to_string(), Path::root()))
                        } else {
                            Err(TreeError::not_found(path.as_str()))
                        };
                    }
                    scope.ensure_reachable_path(&root, path)?;
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
                if !scope.serves_mount(&mount) {
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
                scope.ensure_reachable_path(&mount, &rel)?;
                Ok((mount, rel))
            },
        }
    }

    pub(crate) fn is_mount_enumeration_root(&self, mount: &str, path: &Path) -> bool {
        matches!(&self.backing, Backing::Registry { registry, scope }
            if Self::serves_enumeration_root(registry, scope))
            && mount == MOUNT_ENUMERATION_MOUNT
            && path.is_root()
    }

    pub(crate) fn mount_names(&self) -> Option<Vec<String>> {
        match &self.backing {
            Backing::Registry { registry, scope }
                if Self::serves_enumeration_root(registry, scope) =>
            {
                let mut mounts = if registry.root_mount_name().is_some() {
                    Vec::new()
                } else {
                    registry
                        .mounts()
                        .into_iter()
                        .filter(|mount| scope.serves_mount(mount))
                        .collect()
                };
                mounts.sort();
                Some(mounts)
            },
            Backing::Registry { .. } | Backing::Single { .. } => None,
        }
    }

    pub(crate) fn scope_directory_child(&self, mount: &str, path: &Path) -> Option<String> {
        match &self.backing {
            Backing::Registry { scope, .. } => {
                scope.directory_child(mount, path).map(str::to_owned)
            },
            Backing::Single { .. } => None,
        }
    }

    pub(crate) fn scope_child_resolution(
        &self,
        mount: &str,
        target: &Path,
    ) -> Result<ScopeChildResolution> {
        match &self.backing {
            Backing::Registry { scope, .. } => scope.child_resolution(mount, target),
            Backing::Single { .. } => Ok(ScopeChildResolution::Provider),
        }
    }

    fn serves_enumeration_root(registry: &MountRuntimes, scope: &Scope) -> bool {
        match registry.root_mount_name() {
            Some(root) => !scope.serves_mount(&root),
            None => true,
        }
    }
}

pub(crate) enum ScopeChildResolution {
    Provider,
    SyntheticDirectory,
}

#[derive(Debug, Clone)]
enum Scope {
    All,
    Worldview(BTreeMap<String, MountScope>),
}

impl Scope {
    fn from_worldview(worldview: &Worldview) -> Self {
        Self::Worldview(
            worldview
                .mounts
                .iter()
                .map(|mount| (mount.mount.clone(), MountScope::new(mount.subtree.clone())))
                .collect(),
        )
    }

    fn serves_mount(&self, mount: &str) -> bool {
        match self {
            Self::All => true,
            Self::Worldview(mounts) => mounts.contains_key(mount),
        }
    }

    fn ensure_reachable_path(&self, mount: &str, path: &Path) -> Result<()> {
        match self.classify(mount, path) {
            ScopePath::Provider | ScopePath::SyntheticDirectory => Ok(()),
            ScopePath::NotFound => Err(TreeError::not_found(path.as_str())),
        }
    }

    fn ensure_provider_path(&self, mount: &str, path: &Path) -> Result<()> {
        match self.classify(mount, path) {
            ScopePath::Provider => Ok(()),
            ScopePath::SyntheticDirectory | ScopePath::NotFound => {
                Err(TreeError::not_found(path.as_str()))
            },
        }
    }

    fn directory_child(&self, mount: &str, path: &Path) -> Option<&str> {
        match self {
            Self::All => None,
            Self::Worldview(mounts) => mounts.get(mount)?.directory_child(path),
        }
    }

    fn child_resolution(&self, mount: &str, target: &Path) -> Result<ScopeChildResolution> {
        match self.classify(mount, target) {
            ScopePath::Provider => Ok(ScopeChildResolution::Provider),
            ScopePath::SyntheticDirectory => Ok(ScopeChildResolution::SyntheticDirectory),
            ScopePath::NotFound => Err(TreeError::not_found(target.as_str())),
        }
    }

    fn classify(&self, mount: &str, path: &Path) -> ScopePath {
        match self {
            Self::All => ScopePath::Provider,
            Self::Worldview(mounts) => mounts
                .get(mount)
                .map_or(ScopePath::NotFound, |scope| scope.classify(path)),
        }
    }
}

#[derive(Debug, Clone)]
struct MountScope {
    subtree: Option<Path>,
}

impl MountScope {
    fn new(subtree: Option<Path>) -> Self {
        Self {
            subtree: subtree.filter(|path| !path.is_root()),
        }
    }

    fn classify(&self, path: &Path) -> ScopePath {
        let Some(prefix) = &self.subtree else {
            return ScopePath::Provider;
        };
        if path.has_prefix(prefix) {
            return ScopePath::Provider;
        }
        if prefix.has_prefix(path) {
            return ScopePath::SyntheticDirectory;
        }
        ScopePath::NotFound
    }

    fn directory_child(&self, path: &Path) -> Option<&str> {
        let prefix = self.subtree.as_ref()?;
        if self.classify(path) != ScopePath::SyntheticDirectory {
            return None;
        }
        prefix.segments().nth(path.segments().count())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopePath {
    Provider,
    SyntheticDirectory,
    NotFound,
}
