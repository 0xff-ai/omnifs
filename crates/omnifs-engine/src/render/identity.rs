use dashmap::DashMap;
use dashmap::mapref::one::{Ref, RefMut};
use omnifs_core::path::Path;
use omnifs_workspace::mounts::{Name as MountName, NameError as MountNameError};
use std::hash::Hash;

use crate::view::{EntryKind, EntryMeta, FileAttrsCache, FileSize};

pub type PathToInode<Body, Kind = EntryKind, Extra = ()> =
    IdentityTable<u64, Body, PathKey, Kind, Extra>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PathKey {
    pub mount: MountName,
    pub path: Path,
}

impl PathKey {
    pub fn new(mount: MountName, path: Path) -> Self {
        Self { mount, path }
    }

    pub fn with_mount_str(mount: &str, path: Path) -> Result<Self, MountNameError> {
        Ok(Self::new(MountName::try_from(mount)?, path))
    }
}

pub trait IdentityBody: Clone {
    fn is_provider_resolution(&self) -> bool;

    fn is_synthetic_marker(&self) -> bool;

    fn preserves_over_provider_resolution(&self) -> bool {
        false
    }

    fn clears_attrs_and_forces_exact_size(&self) -> bool {
        false
    }

    fn merge_identity_body(&mut self, incoming: Self) {
        if incoming.is_provider_resolution() && self.preserves_over_provider_resolution() {
            return;
        }
        if incoming.is_provider_resolution() || !self.is_provider_resolution() {
            *self = incoming;
            return;
        }
        if !incoming.is_synthetic_marker() {
            *self = incoming;
        }
    }
}

pub trait IdentityKind: Copy + PartialEq {
    fn is_file(self) -> bool;
}

impl IdentityKind for EntryKind {
    fn is_file(self) -> bool {
        self == EntryKind::File
    }
}

#[derive(Debug, Clone)]
pub enum BodyUpdate<Body> {
    Keep,
    Merge(Body),
}

#[derive(Debug, Clone)]
pub struct IdentitySeed<Kind, Body, Extra = ()> {
    pub mount_name: String,
    pub path: Path,
    pub kind: Kind,
    pub attrs: Option<FileAttrsCache>,
    pub size: u64,
    pub size_exact: bool,
    pub body: Body,
    pub body_update: BodyUpdate<Body>,
    pub extra: Extra,
}

impl<Kind, Body> IdentitySeed<Kind, Body> {
    pub fn new(
        mount_name: impl Into<String>,
        path: Path,
        kind: Kind,
        attrs: Option<FileAttrsCache>,
        size: u64,
        body: Body,
    ) -> Self
    where
        Kind: Copy,
        Body: Clone,
    {
        let size_exact = attrs
            .as_ref()
            .is_none_or(|attrs| matches!(attrs.size(), FileSize::Exact(_)));
        Self {
            mount_name: mount_name.into(),
            path,
            kind,
            attrs,
            size,
            size_exact,
            body: body.clone(),
            body_update: BodyUpdate::Merge(body),
            extra: (),
        }
    }
}

impl<Kind, Body, Extra> IdentitySeed<Kind, Body, Extra>
where
    Body: Clone,
{
    #[must_use]
    pub fn keep_body_on_refresh(mut self) -> Self {
        self.body_update = BodyUpdate::Keep;
        self
    }

    #[must_use]
    pub fn with_body_update(mut self, body_update: BodyUpdate<Body>) -> Self {
        self.body_update = body_update;
        self
    }
}

#[derive(Debug, Clone)]
pub struct IdentityEntry<Kind, Body, Extra = ()> {
    pub mount_name: String,
    pub path: Path,
    pub kind: Kind,
    pub attrs: Option<FileAttrsCache>,
    pub size: u64,
    pub size_exact: bool,
    pub body: Body,
    pub extra: Extra,
}

impl<Kind, Body, Extra> IdentityEntry<Kind, Body, Extra>
where
    Kind: IdentityKind,
    Body: IdentityBody,
    Extra: Clone,
{
    fn from_seed(seed: IdentitySeed<Kind, Body, Extra>) -> Self {
        Self {
            mount_name: seed.mount_name,
            path: seed.path,
            kind: seed.kind,
            attrs: seed.attrs,
            size: seed.size,
            size_exact: seed.size_exact,
            body: seed.body,
            extra: seed.extra,
        }
    }

    fn refresh(&mut self, seed: IdentitySeed<Kind, Body, Extra>) {
        let attrs = self.refreshed_attrs(seed.kind, seed.attrs);
        let attrs_size = attrs.as_ref().map(FileAttrsCache::st_size);
        let attrs_exact = attrs
            .as_ref()
            .map(|attrs| matches!(attrs.size(), FileSize::Exact(_)));

        self.mount_name = seed.mount_name;
        self.path = seed.path;
        self.kind = seed.kind;
        self.attrs = attrs;
        self.extra = seed.extra;

        if let Some(size) = attrs_size {
            self.size = size;
            self.size_exact = attrs_exact.unwrap_or(false);
        } else if seed.size_exact || !self.size_exact {
            self.size = seed.size;
            self.size_exact = seed.size_exact;
        }

        match seed.body_update {
            BodyUpdate::Keep => {},
            BodyUpdate::Merge(body) => self.body.merge_identity_body(body),
        }

        if self.body.clears_attrs_and_forces_exact_size() {
            self.attrs = None;
            self.size_exact = true;
        }
    }

    fn refreshed_attrs(
        &self,
        incoming_kind: Kind,
        incoming_attrs: Option<FileAttrsCache>,
    ) -> Option<FileAttrsCache> {
        match FileAttrsCache::merge_preserving_learned_size(self.attrs.as_ref(), incoming_attrs) {
            Some(attrs) => Some(attrs),
            None if self.kind.is_file() && incoming_kind.is_file() => self.attrs.clone(),
            None => None,
        }
    }
}

impl<Body, Extra> IdentityEntry<EntryKind, Body, Extra> {
    pub fn meta(&self) -> EntryMeta {
        match self.kind {
            EntryKind::Directory => EntryMeta::directory(),
            EntryKind::File => match self.attrs.clone() {
                Some(attrs) => EntryMeta::file(attrs),
                None => EntryMeta::file_without_attrs(),
            },
        }
    }
}

pub struct IdentityTable<Id, Body, Key = PathKey, Kind = EntryKind, Extra = ()> {
    key_to_id: DashMap<Key, Id>,
    entries: DashMap<Id, IdentityEntry<Kind, Body, Extra>>,
}

impl<Id, Body, Key, Kind, Extra> Default for IdentityTable<Id, Body, Key, Kind, Extra>
where
    Id: Copy + Eq + Hash,
    Body: IdentityBody,
    Key: Clone + Eq + Hash,
    Kind: IdentityKind,
    Extra: Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<Id, Body, Key, Kind, Extra> IdentityTable<Id, Body, Key, Kind, Extra>
where
    Id: Copy + Eq + Hash,
    Body: IdentityBody,
    Key: Clone + Eq + Hash,
    Kind: IdentityKind,
    Extra: Clone,
{
    pub fn new() -> Self {
        Self {
            key_to_id: DashMap::new(),
            entries: DashMap::new(),
        }
    }

    pub fn entries(&self) -> &DashMap<Id, IdentityEntry<Kind, Body, Extra>> {
        &self.entries
    }

    pub fn key_to_id(&self) -> &DashMap<Key, Id> {
        &self.key_to_id
    }

    pub fn get(&self, id: &Id) -> Option<Ref<'_, Id, IdentityEntry<Kind, Body, Extra>>> {
        self.entries.get(id)
    }

    pub fn get_mut(&self, id: &Id) -> Option<RefMut<'_, Id, IdentityEntry<Kind, Body, Extra>>> {
        self.entries.get_mut(id)
    }

    pub fn insert_entry(&self, id: Id, entry: IdentityEntry<Kind, Body, Extra>) {
        self.entries.insert(id, entry);
    }

    pub fn insert_key(&self, key: Key, id: Id) {
        self.key_to_id.insert(key, id);
    }

    pub fn remove_id(&self, id: Id) -> Option<IdentityEntry<Kind, Body, Extra>> {
        self.entries.remove(&id).map(|(_, entry)| entry)
    }

    pub fn remove_key(&self, key: &Key) -> Option<Id> {
        self.key_to_id.remove(key).map(|(_, id)| id)
    }

    pub fn id_for_key(&self, key: &Key) -> Option<Id> {
        self.key_to_id.get(key).map(|entry| *entry.value())
    }

    pub fn contains_id(&self, id: Id) -> bool {
        self.entries.contains_key(&id)
    }

    pub fn get_or_alloc(
        &self,
        key: Key,
        seed: IdentitySeed<Kind, Body, Extra>,
        alloc: impl FnOnce() -> Id,
    ) -> Id {
        let refresh_seed = seed.clone();
        *self
            .key_to_id
            .entry(key)
            .and_modify(|existing_id| {
                if let Some(mut entry) = self.entries.get_mut(existing_id) {
                    entry.refresh(refresh_seed);
                }
            })
            .or_insert_with(|| {
                let id = alloc();
                self.entries.insert(id, IdentityEntry::from_seed(seed));
                id
            })
    }
}
