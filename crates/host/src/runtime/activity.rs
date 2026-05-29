use crate::omnifs::provider::types::ActivePathSet;
use crate::path_prefix::path_prefix_matches;
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct ActiveMountEntry {
    mount_name: String,
    paths: HashMap<String, Instant>,
}

#[derive(Debug, Clone)]
pub struct ActivePathTouch {
    pub mount_id: String,
    pub mount_name: String,
    pub path: String,
}

pub struct ActivityTable {
    ttl: Duration,
    entries: HashMap<String, ActiveMountEntry>,
}

impl ActivityTable {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: HashMap::new(),
        }
    }

    pub fn touch<I>(&mut self, touched: I)
    where
        I: IntoIterator<Item = ActivePathTouch>,
    {
        let now = Instant::now();
        for ActivePathTouch {
            mount_id,
            mount_name,
            path,
        } in touched
        {
            self.entries
                .entry(mount_id)
                .or_insert_with(|| ActiveMountEntry {
                    mount_name,
                    paths: HashMap::new(),
                })
                .paths
                .insert(path, now);
        }
    }

    pub fn active_path_sets(&mut self) -> Vec<ActivePathSet> {
        self.prune();

        let mut active = self
            .entries
            .iter()
            .map(|(mount_id, entry)| {
                let mut paths = entry.paths.keys().cloned().collect::<Vec<_>>();
                paths.sort();
                ActivePathSet {
                    mount_id: mount_id.clone(),
                    mount_name: entry.mount_name.clone(),
                    paths,
                }
            })
            .collect::<Vec<_>>();
        active.sort_by(|left, right| left.mount_id.cmp(&right.mount_id));
        active
    }

    pub fn remove_path(&mut self, path: &str) {
        self.entries.retain(|_, entry| {
            entry.paths.remove(path);
            !entry.paths.is_empty()
        });
    }

    pub fn remove_prefix(&mut self, prefix: &str) {
        self.entries.retain(|_, entry| {
            entry
                .paths
                .retain(|path, _| !path_prefix_matches(prefix, path));
            !entry.paths.is_empty()
        });
    }

    fn prune(&mut self) {
        let now = Instant::now();
        let ttl = self.ttl;
        self.entries.retain(|_, entry| {
            entry
                .paths
                .retain(|_, last_touch| now.saturating_duration_since(*last_touch) < ttl);
            !entry.paths.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{ActivePathTouch, ActivityTable};
    use std::time::Duration;
    #[test]
    fn activity_table_removes_exact_paths_and_prefixes() {
        let mut table = ActivityTable::new(Duration::from_secs(60));
        table.touch([
            ActivePathTouch {
                mount_id: "/{owner}/{repo}".to_string(),
                mount_name: "Repo".to_string(),
                path: "/openai/gvfs".to_string(),
            },
            ActivePathTouch {
                mount_id: "/{owner}/{repo}/issues/open/{number}".to_string(),
                mount_name: "Issue".to_string(),
                path: "/openai/gvfs/issues/open/7".to_string(),
            },
            ActivePathTouch {
                mount_id: "/{owner}/{repo}/issues/open/{number}".to_string(),
                mount_name: "Issue".to_string(),
                path: "/openai/gvfs/issues/open/8".to_string(),
            },
        ]);

        table.remove_path("/openai/gvfs/issues/open/7");
        let active = table.active_path_sets();
        let issue = active
            .iter()
            .find(|entry| entry.mount_id == "/{owner}/{repo}/issues/open/{number}")
            .expect("missing issue activity");
        assert_eq!(issue.paths, vec!["/openai/gvfs/issues/open/8"]);

        table.remove_prefix("/openai/gvfs");
        assert!(table.active_path_sets().is_empty());
    }

    #[test]
    fn activity_table_prefix_delete_respects_boundaries_and_root() {
        let mut table = ActivityTable::new(Duration::from_secs(60));
        table.touch([
            ActivePathTouch {
                mount_id: "/{owner}/{repo}".to_string(),
                mount_name: "Repo".to_string(),
                path: "/openai/gvfs".to_string(),
            },
            ActivePathTouch {
                mount_id: "/{owner}/{repo}".to_string(),
                mount_name: "Repo".to_string(),
                path: "/openai/gvfs-tools".to_string(),
            },
        ]);

        table.remove_prefix("/openai/gvfs");
        let active = table.active_path_sets();
        assert_eq!(active[0].paths, vec!["/openai/gvfs-tools"]);

        table.remove_prefix("/");
        assert!(table.active_path_sets().is_empty());
    }
}
