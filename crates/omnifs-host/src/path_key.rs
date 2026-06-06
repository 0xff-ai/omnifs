use dashmap::DashMap;

pub type PathToInode = DashMap<PathKey, u64>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PathKey {
    pub mount: String,
    pub path: String,
}

impl PathKey {
    pub fn new(mount: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            mount: mount.into(),
            path: path.into(),
        }
    }
}
