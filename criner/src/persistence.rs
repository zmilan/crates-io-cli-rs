use crate::model::CrateVersion;
use crate::{
    error::{Error, Result},
    model::{Context, Crate},
};
use sled::{IVec, Tree};
use std::{path::Path, time::SystemTime};

#[derive(Clone)]
pub struct Db {
    pub inner: sled::Db,
    meta: sled::Tree,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        // NOTE: Default compression achieves cutting disk space in half, but halfs the processing speed
        // for our binary data as well.
        // TODO: re-evaluate that for textual data - it might enable us to store all files, and when we
        // have more read-based workloads. Maybe it's worth it to turn on.
        // NOTE: Databases with and without compression need migration.
        let inner = sled::Config::new().path(path).open()?;
        let meta = inner.open_tree("meta")?;
        Ok(Db { inner, meta })
    }

    pub fn open_crate_versions(&self) -> Result<CrateVersionsTree> {
        Ok(CrateVersionsTree {
            inner: self.inner.open_tree("crate_versions")?,
        })
    }

    pub fn open_crates(&self) -> Result<CratesTree> {
        Ok(CratesTree {
            inner: self.inner.open_tree("crates")?,
        })
    }

    pub fn context(&self) -> Result<ContextTree> {
        Ok(ContextTree { inner: &self.meta })
    }
}

pub trait TreeAccess {
    type StorageItem: From<IVec> + Into<IVec> + for<'a> From<&'a [u8]> + Default;
    type InsertItem: serde::Serialize;
    type InsertResult;

    fn tree(&self) -> &sled::Tree;
    fn key(&self, item: &Self::InsertItem) -> Vec<u8>;
    fn map_insert_return_value(&self, v: IVec) -> Self::InsertResult;
    fn merge(
        &self,
        new_item: &Self::InsertItem,
        existing_item: Option<Self::StorageItem>,
    ) -> Option<Self::StorageItem>;

    /// Update an existing item, or create it as default, returning the stored item
    fn update(
        &self,
        key: impl AsRef<[u8]>,
        f: impl Fn(&mut Self::StorageItem),
    ) -> Result<Self::StorageItem> {
        self.tree()
            .update_and_fetch(key, |bytes: Option<&[u8]>| {
                Some(match bytes {
                    Some(bytes) => {
                        let mut v = bytes.into();
                        f(&mut v);
                        v.into()
                    }
                    None => Self::StorageItem::default().into(),
                })
            })?
            .map(From::from)
            .ok_or_else(|| Error::Bug("We always set a value"))
    }

    /// Similar to 'update', but provides full control over the default
    fn upsert(&self, item: &Self::InsertItem) -> Result<Self::InsertResult> {
        self.tree()
            .update_and_fetch(self.key(item), |existing: Option<&[u8]>| {
                self.merge(item, existing.map(From::from)).map(Into::into)
            })?
            .ok_or_else(|| Error::Bug("We always put a value or update the existing one"))
            .map(|v| self.map_insert_return_value(v))
    }

    fn insert(&self, v: &Self::InsertItem) -> Result<()> {
        self.tree()
            .insert(self.key(v), rmp_serde::to_vec(v)?)
            .map_err(Error::from)
            .map(|_| ())
    }
}

pub struct ContextTree<'a> {
    inner: &'a sled::Tree,
}

impl<'a> TreeAccess for ContextTree<'a> {
    type StorageItem = Context;
    type InsertItem = Context;
    type InsertResult = ();

    fn tree(&self) -> &Tree {
        self.inner
    }

    fn key(&self, _item: &Self::InsertItem) -> Vec<u8> {
        format!(
            "context/{}",
            humantime::format_rfc3339(SystemTime::now())
                .to_string()
                .get(..10)
                .expect("YYYY-MM-DD - 10 bytes")
        )
        .into()
    }

    fn map_insert_return_value(&self, _v: IVec) -> Self::InsertResult {
        ()
    }

    fn merge(&self, new: &Context, existing_item: Option<Context>) -> Option<Self::StorageItem> {
        existing_item
            .map(|existing| existing + new)
            .or_else(|| Some(new.clone()))
    }
}

impl<'a> ContextTree<'a> {
    pub fn update_today(&self, f: impl Fn(&mut Context)) -> Result<Context> {
        self.update(self.key(&Context::default()), f)
    }

    // NOTE: impl iterator is not allowed in traits unfortunately, but one could implement one manually
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Result<(String, Context)>> {
        self.inner.iter().map(|r| {
            r.map(|(k, v)| {
                (
                    String::from_utf8(k.as_ref().to_vec()).expect("utf8"),
                    Context::from(v),
                )
            })
            .map_err(Error::from)
        })
    }
}

#[derive(Clone)]
pub struct CratesTree {
    inner: sled::Tree,
}

impl TreeAccess for CratesTree {
    type StorageItem = Crate;
    type InsertItem = crates_index_diff::CrateVersion;
    type InsertResult = bool;

    fn tree(&self) -> &Tree {
        &self.inner
    }

    fn key(&self, item: &crates_index_diff::CrateVersion) -> Vec<u8> {
        item.name.clone().into_bytes()
    }

    fn map_insert_return_value(&self, v: IVec) -> Self::InsertResult {
        let c = Crate::from(v);
        c.versions.len() == 1
    }

    fn merge(
        &self,
        new_item: &crates_index_diff::CrateVersion,
        existing_item: Option<Crate>,
    ) -> Option<Crate> {
        Some(match existing_item {
            Some(mut c) => {
                // NOTE: We assume that a version can only be added once! They are immutable.
                // However, idempotence is more important
                if !c.versions.contains(&new_item.version) {
                    c.versions.push(new_item.version.to_owned());
                }
                c.versions.sort();
                c
            }
            None => Crate::from(new_item),
        })
    }
}

#[derive(Clone)]
pub struct CrateVersionsTree {
    inner: sled::Tree,
}

impl TreeAccess for CrateVersionsTree {
    type StorageItem = CrateVersion;
    type InsertItem = crates_index_diff::CrateVersion;
    type InsertResult = ();

    fn tree(&self) -> &Tree {
        &self.inner
    }

    fn key(&self, v: &crates_index_diff::CrateVersion) -> Vec<u8> {
        let mut id = Vec::with_capacity(v.name.len() + v.version.len() + 1);
        id.extend_from_slice(&v.name.as_bytes());
        id.push(b':');
        id.extend_from_slice(&v.version.as_bytes());
        id
    }

    fn map_insert_return_value(&self, _v: IVec) -> Self::InsertResult {
        ()
    }

    fn merge(
        &self,
        new_item: &Self::InsertItem,
        _existing_item: Option<CrateVersion>,
    ) -> Option<Self::StorageItem> {
        Some(new_item.into())
    }
}

macro_rules! impl_ivec_transform {
    ($ty:ty) => {
        impl From<&[u8]> for $ty {
            fn from(b: &[u8]) -> Self {
                rmp_serde::from_read(b).expect("always valid decoding: TODO: migrations")
            }
        }
        impl From<IVec> for $ty {
            fn from(b: IVec) -> Self {
                rmp_serde::from_read(b.as_ref()).expect("always valid decoding: TODO: migrations")
            }
        }
        impl From<$ty> for IVec {
            fn from(c: $ty) -> Self {
                rmp_serde::to_vec(&c)
                    .expect("serialization to always succeed")
                    .into()
            }
        }
    };
}

impl_ivec_transform!(Crate);
impl_ivec_transform!(CrateVersion);
impl_ivec_transform!(Context);
