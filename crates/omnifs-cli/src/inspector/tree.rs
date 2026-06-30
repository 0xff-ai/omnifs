//! Live projection of the filesystem the user is browsing.
//!
//! A `PathTree` mirrors the omnifs projected filesystem as the user
//! touches it. Mount roots are first-level children of the synthetic
//! root; below that, nodes are added on demand as the FUSE layer
//! resolves paths.
//!
//! The tree owns *display state* (last touch, in-flight set, latency,
//! status), not authoritative filesystem state. The host caches remain
//! the source of truth; this is a UX projection.

use std::collections::{BTreeMap, HashSet};

use omnifs_inspector::TraceId;

/// How recent a node's last touch has to be to stay in the visible
/// tree under active-focus pruning. Tunable: small enough to keep
/// long-completed activity from cluttering the screen, large enough
/// that intermediate-paced browsing doesn't lose its breadcrumbs.
pub const ACTIVE_FOCUS_WINDOW_US: u64 = 30_000_000;

/// One node in the live path tree. Children are stored in a `BTreeMap`
/// so render order is deterministic and alphabetical without an extra
/// sort pass.
#[derive(Debug, Clone)]
pub struct PathNode {
    /// Last segment of the path; used for rendering.
    pub name: String,
    /// Full path under the mount (empty at the mount root).
    pub mount_relative_path: String,
    /// Children indexed by name.
    pub children: BTreeMap<String, PathNode>,
    /// What we currently know about this node.
    pub status: NodeStatus,
    /// Monotonic micros of the last event that touched this node.
    pub last_touched_mono: u64,
    /// Latency of the most recent completed op at this node, in micros.
    pub last_latency_us: Option<u64>,
    /// Trace IDs currently in-flight at this exact node.
    pub in_flight: HashSet<TraceId>,
    /// True when a subtree handoff lookup returned this path; reads
    /// below this node bypass the provider entirely.
    pub is_subtree_handoff: bool,
    /// User has manually collapsed this subtree.
    pub manually_collapsed: bool,
}

impl PathNode {
    fn new(name: impl Into<String>, mount_relative_path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            mount_relative_path: mount_relative_path.into(),
            children: BTreeMap::new(),
            status: NodeStatus::Untouched,
            last_touched_mono: 0,
            last_latency_us: None,
            in_flight: HashSet::new(),
            is_subtree_handoff: false,
            manually_collapsed: false,
        }
    }

    /// Highest-priority status across this node and its descendants;
    /// used for the parent's badge when a subtree is collapsed.
    pub fn rollup_status(&self) -> NodeStatus {
        let mut best = self.status;
        for child in self.children.values() {
            best = best.max(child.rollup_status());
        }
        best
    }

    /// In-flight op count at this node plus every descendant.
    pub fn in_flight_count(&self) -> u32 {
        let here = u32::try_from(self.in_flight.len()).unwrap_or(u32::MAX);
        let descendants: u32 = self
            .children
            .values()
            .map(PathNode::in_flight_count)
            .fold(0u32, u32::saturating_add);
        here.saturating_add(descendants)
    }

    /// Count of descendants currently in the `Error` state, excluding
    /// this node itself. Surfaces a small `✗N` badge on ancestors so
    /// failures are discoverable from the top without painting every
    /// ancestor red.
    pub fn error_count_below(&self) -> u32 {
        self.children
            .values()
            .map(|child| {
                let here = u32::from(matches!(child.status, NodeStatus::Error));
                here.saturating_add(child.error_count_below())
            })
            .fold(0u32, u32::saturating_add)
    }

    /// Total descendants (children + grand-children + …), not counting
    /// this node. Used to size the `… N nodes` collapsed-summary row.
    pub fn subtree_count(&self) -> usize {
        self.children
            .values()
            .map(|child| 1 + child.subtree_count())
            .sum()
    }

    /// True when this node should be visible under the active-focus
    /// policy: an in-flight op, an error, a recent touch, or any
    /// descendant matching those.
    pub fn is_in_active_focus(&self, now_mono: u64, window_us: u64) -> bool {
        if !self.in_flight.is_empty() || matches!(self.status, NodeStatus::Error) {
            return true;
        }
        let touched_recently = now_mono.saturating_sub(self.last_touched_mono) <= window_us;
        if touched_recently && self.last_touched_mono > 0 {
            return true;
        }
        self.children
            .values()
            .any(|child| child.is_in_active_focus(now_mono, window_us))
    }
}

/// Glyph state for a node. Ordered so `max()` picks the most
/// attention-worthy variant when rolling up a subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NodeStatus {
    Untouched,
    Cached,
    RecentHit,
    InFlight,
    Error,
}

impl NodeStatus {
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Untouched => "□",
            Self::Cached => "▣",
            Self::RecentHit => "◐",
            Self::InFlight => "▦",
            Self::Error => "✗",
        }
    }
}

/// Per-mount path tree. The mount name is implicit (one tree per mount
/// in `MountForest`).
#[derive(Debug, Clone)]
pub struct MountTree {
    pub mount: String,
    pub root: PathNode,
    pub last_activity_mono: u64,
}

impl MountTree {
    pub fn new(mount: impl Into<String>) -> Self {
        let mount = mount.into();
        Self {
            root: PathNode::new(mount.clone(), ""),
            mount,
            last_activity_mono: 0,
        }
    }

    /// Walk to the node at `path`, creating ancestors as needed.
    /// Ancestors and the mount root are promoted from `Untouched` to
    /// `Cached` along the way — they're known to exist by virtue of
    /// the walk, so showing them as never-touched would be misleading.
    fn ensure(&mut self, path: &str) -> &mut PathNode {
        if self.root.status == NodeStatus::Untouched {
            self.root.status = NodeStatus::Cached;
        }
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut node = &mut self.root;
        let mut so_far = String::new();
        let last_idx = segments.len().saturating_sub(1);
        for (i, segment) in segments.iter().enumerate() {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(segment);
            let key = (*segment).to_string();
            let segment_path = so_far.clone();
            node = node
                .children
                .entry(key.clone())
                .or_insert_with(|| PathNode::new(key, segment_path));
            // Promote intermediates; leave the leaf alone — the caller
            // is about to set its real status.
            if i < last_idx && node.status == NodeStatus::Untouched {
                node.status = NodeStatus::Cached;
            }
        }
        node
    }

    /// Mark a path as in-flight for `trace_id`. Walks the tree creating
    /// nodes as needed and bumps `last_touched_mono` for the leaf.
    pub fn mark_in_flight(&mut self, path: &str, trace_id: TraceId, mono_us: u64) {
        self.last_activity_mono = mono_us;
        let node = self.ensure(path);
        node.in_flight.insert(trace_id);
        node.status = NodeStatus::InFlight;
        node.last_touched_mono = mono_us;
    }

    /// Mark completion. Demotes from `InFlight` to `Cached` (success)
    /// or `Error` (failure). Clears the in-flight trace marker.
    pub fn complete(
        &mut self,
        path: &str,
        trace_id: TraceId,
        latency_us: u64,
        ok: bool,
        mono_us: u64,
    ) {
        self.last_activity_mono = mono_us;
        let node = self.ensure(path);
        node.in_flight.remove(&trace_id);
        node.last_latency_us = Some(latency_us);
        node.last_touched_mono = mono_us;
        node.status = if !ok {
            NodeStatus::Error
        } else if node.in_flight.is_empty() {
            NodeStatus::Cached
        } else {
            NodeStatus::InFlight
        };
    }

    /// Mark a transient cache-hit flash. Promotes the node to `Cached`
    /// (it must be cached if we got a hit) and stamps a touch.
    pub fn cache_hit(&mut self, path: &str, mono_us: u64) {
        self.last_activity_mono = mono_us;
        let node = self.ensure(path);
        node.status = NodeStatus::RecentHit;
        node.last_touched_mono = mono_us;
    }

    /// Mark this node as a subtree handoff target.
    pub fn mark_subtree_handoff(&mut self, path: &str, mono_us: u64) {
        self.last_activity_mono = mono_us;
        let node = self.ensure(path);
        node.is_subtree_handoff = true;
        node.last_touched_mono = mono_us;
    }
}

/// Collection of per-mount trees plus a synthetic forest root.
#[derive(Debug, Default, Clone)]
pub struct MountForest {
    mounts: BTreeMap<String, MountTree>,
}

impl MountForest {
    pub fn mount_tree_mut(&mut self, mount: &str) -> &mut MountTree {
        self.mounts
            .entry(mount.to_string())
            .or_insert_with(|| MountTree::new(mount))
    }

    /// Toggle the manual collapse flag on the node at `(mount, path)`.
    /// Returns the new flag; no-op (returns `None`) when the node
    /// doesn't exist.
    pub fn toggle_collapsed(&mut self, mount: &str, path: &str) -> Option<bool> {
        let tree = self.mounts.get_mut(mount)?;
        let node = lookup_node_mut(&mut tree.root, path)?;
        node.manually_collapsed = !node.manually_collapsed;
        Some(node.manually_collapsed)
    }
}

fn lookup_node_mut<'a>(root: &'a mut PathNode, path: &str) -> Option<&'a mut PathNode> {
    if path.is_empty() {
        return Some(root);
    }
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut node = root;
    for segment in segments {
        node = node.children.get_mut(segment)?;
    }
    Some(node)
}

impl MountForest {
    pub fn iter(&self) -> impl Iterator<Item = &MountTree> {
        self.mounts.values()
    }

    /// Flatten into render rows under the active-focus policy:
    ///
    ///   - Mount roots are always shown.
    ///   - A child node is shown when itself or any descendant was
    ///     touched within `recent_window_us`, OR has an in-flight op,
    ///     OR is an error.
    ///   - Subtrees collapsed by the user or out-of-window collapse
    ///     into a single summary row with descendant counts.
    pub fn render_rows(&self, now_mono: u64, recent_window_us: u64) -> Vec<RenderRow> {
        // Order mounts by recency so the busiest sit at the top of the
        // tree, matching the sparkline strip's ordering.
        let mut trees: Vec<&MountTree> = self.mounts.values().collect();
        trees.sort_by_key(|t| std::cmp::Reverse(t.last_activity_mono));
        let mut rows = Vec::new();
        for tree in trees {
            // Mount roots show their own state (Untouched unless the
            // root path itself is touched), not a rolled-up one — that
            // would let a single errored leaf paint the whole mount red.
            // Error/in-flight counts surface as separate badges.
            rows.push(RenderRow {
                depth: 0,
                name: tree.mount.clone(),
                path: String::new(),
                mount: tree.mount.clone(),
                status: tree.root.status,
                is_subtree_handoff: false,
                last_latency_us: None,
                in_flight: tree.root.in_flight_count(),
                errors_below: tree.root.error_count_below(),
            });
            tree.root
                .flatten_into(1, &tree.mount, now_mono, recent_window_us, &mut rows);
        }
        rows
    }
}

/// One flattened row ready to be turned into a Line by the renderer.
#[derive(Debug, Clone)]
pub struct RenderRow {
    pub depth: usize,
    pub name: String,
    /// Stable path identity used by keyboard navigation and selection sync.
    pub path: String,
    pub mount: String,
    pub status: NodeStatus,
    pub is_subtree_handoff: bool,
    pub last_latency_us: Option<u64>,
    pub in_flight: u32,
    pub errors_below: u32,
}

/// Synthetic path suffix that marks a collapsed-summary row as
/// distinct from the parent it summarises. Without this, the parent
/// and the summary share `(mount, path)` identity and the keyboard
/// cursor can't step past the summary into the next sibling.
pub const COLLAPSED_SUMMARY_SUFFIX: &str = "\x1f__collapsed__";

impl PathNode {
    /// Recursively append rows for this node's descendants. Skips
    /// children outside the active-focus window. When this node is
    /// manually collapsed, pushes a single `… N nodes` summary row
    /// (with a sentinel path so the cursor can step past it) instead
    /// of recursing into children.
    fn flatten_into(
        &self,
        depth: usize,
        mount: &str,
        now_mono: u64,
        window_us: u64,
        out: &mut Vec<RenderRow>,
    ) {
        if self.manually_collapsed {
            self.push_collapsed_summary(depth, mount, out);
            return;
        }
        for child in self.children.values() {
            if !child.is_in_active_focus(now_mono, window_us) {
                continue;
            }
            out.push(RenderRow {
                depth,
                name: child.name.clone(),
                path: child.mount_relative_path.clone(),
                mount: mount.to_string(),
                status: child.status,
                is_subtree_handoff: child.is_subtree_handoff,
                last_latency_us: child.last_latency_us,
                in_flight: child.in_flight_count(),
                errors_below: child.error_count_below(),
            });
            child.flatten_into(depth + 1, mount, now_mono, window_us, out);
        }
    }

    fn push_collapsed_summary(&self, depth: usize, mount: &str, out: &mut Vec<RenderRow>) {
        let count = self.subtree_count();
        if count == 0 {
            return;
        }
        let summary_path = if self.mount_relative_path.is_empty() {
            COLLAPSED_SUMMARY_SUFFIX.to_string()
        } else {
            format!("{}{}", self.mount_relative_path, COLLAPSED_SUMMARY_SUFFIX)
        };
        out.push(RenderRow {
            depth,
            name: format!("… {count} nodes"),
            path: summary_path,
            mount: mount.to_string(),
            status: self.rollup_status(),
            is_subtree_handoff: self.is_subtree_handoff,
            last_latency_us: None,
            in_flight: self.in_flight_count(),
            errors_below: self.error_count_below(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_demotes_to_cached_when_no_in_flight_left() {
        let mut tree = MountTree::new("dns");
        tree.mark_in_flight("example.com/MX", 1, 100);
        tree.complete("example.com/MX", 1, 2_000, true, 200);
        let node = tree
            .root
            .children
            .get("example.com")
            .and_then(|n| n.children.get("MX"))
            .expect("node");
        assert_eq!(node.status, NodeStatus::Cached);
        assert_eq!(node.last_latency_us, Some(2_000));
        assert!(node.in_flight.is_empty());
    }

    #[test]
    fn render_rows_honors_active_focus_window() {
        let mut forest = MountForest::default();
        let tree = forest.mount_tree_mut("github");
        tree.mark_in_flight("a/recent", 1, 100_000_000);
        tree.complete("a/recent", 1, 1_000, true, 100_000_000);
        tree.mark_in_flight("a/old", 2, 100);
        tree.complete("a/old", 2, 1_000, true, 100);

        let rows = forest.render_rows(100_000_000, 1_000_000);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"github"));
        assert!(names.contains(&"a"));
        assert!(names.contains(&"recent"));
        assert!(
            !names.contains(&"old"),
            "stale leaf should not appear in active focus"
        );
    }

    #[test]
    fn error_status_pins_node_into_visible_set() {
        let mut forest = MountForest::default();
        let tree = forest.mount_tree_mut("arxiv");
        tree.mark_in_flight("2401.99999/title", 1, 100);
        tree.complete("2401.99999/title", 1, 1_000, false, 100);

        let rows = forest.render_rows(10_000_000_000, 1_000_000);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(
            names.contains(&"title"),
            "errored node should stay pinned regardless of age"
        );
    }
}
