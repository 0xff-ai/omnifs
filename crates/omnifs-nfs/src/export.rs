use crate::protocol::consts::{
    ACCESS4_EXECUTE, ACCESS4_LOOKUP, ACCESS4_READ, NF4DIR, NF4LNK, NF4REG, NFS4ERR_RESOURCE,
};

pub type NfsResult<T> = Result<T, u32>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfsNodeKind {
    Directory,
    File,
    Symlink,
}

impl NfsNodeKind {
    pub(crate) fn nfs_type(self) -> u32 {
        match self {
            Self::Directory => NF4DIR,
            Self::File => NF4REG,
            Self::Symlink => NF4LNK,
        }
    }

    pub(crate) fn allowed_access(self) -> u32 {
        match self {
            Self::Directory => ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_EXECUTE,
            Self::File | Self::Symlink => ACCESS4_READ,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NfsAttr {
    pub id: u64,
    pub parent: u64,
    pub kind: NfsNodeKind,
    pub size: u64,
    pub mode: u32,
    pub change: u64,
    pub mtime_sec: i64,
}

#[derive(Debug, Clone)]
pub struct NfsDirEntry {
    pub id: u64,
    pub name: String,
    pub attr: NfsAttr,
}

pub trait ReadOnlyExport: Send + Sync {
    fn root(&self) -> u64;
    fn attr(&self, id: u64) -> NfsResult<NfsAttr>;
    fn lookup(&self, parent: u64, name: &str) -> NfsResult<u64>;
    fn readdir(&self, id: u64) -> NfsResult<Vec<NfsDirEntry>>;
    fn read(&self, id: u64) -> NfsResult<Vec<u8>>;
    fn readlink(&self, id: u64) -> NfsResult<Vec<u8>>;

    fn parent(&self, id: u64) -> NfsResult<u64> {
        Ok(self.attr(id)?.parent)
    }

    fn materialize_for_open(&self, id: u64, limit: usize) -> NfsResult<usize> {
        let data = self.read(id)?;
        if data.len() > limit {
            return Err(NFS4ERR_RESOURCE);
        }
        Ok(data.len())
    }
}
