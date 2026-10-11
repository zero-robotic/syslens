use anyhow::{Context, Result, anyhow};
use std::{
    any::Any,
    collections::HashMap,
    marker::PhantomData,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn timestamp_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

pub struct Key<T> {
    pub name: &'static str,
    marker: PhantomData<fn() -> T>,
}
impl<T> Copy for Key<T> {}
impl<T> Clone for Key<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Key<T> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }
}

#[derive(Clone, Copy)]
pub struct CollectContext {
    pub captured_at_ns: u64,
}
impl CollectContext {
    pub fn now() -> Self {
        Self {
            captured_at_ns: timestamp_ns(),
        }
    }
}

pub(super) struct Entry {
    pub captured_at_ns: u64,
    pub result: Result<Box<dyn Any + Send + Sync>, String>,
}

pub struct Batch {
    pub id: u64,
    pub captured_at_ns: u64,
    pub(super) entries: HashMap<&'static str, Entry>,
}
impl Batch {
    pub fn get<T: Send + Sync + 'static>(&self, key: Key<T>) -> Result<&T> {
        let entry = self
            .entries
            .get(key.name)
            .with_context(|| format!("missing input {}", key.name))?;
        let data = entry
            .result
            .as_ref()
            .map_err(|error| anyhow!("{}: {error}", key.name))?;
        data.downcast_ref::<T>()
            .with_context(|| format!("input type mismatch for {}", key.name))
    }

    pub fn status(&self, name: &str) -> (u64, Option<String>) {
        match self.entries.get(name) {
            Some(entry) => (entry.captured_at_ns, entry.result.as_ref().err().cloned()),
            None => (self.captured_at_ns, Some(format!("missing input {name}"))),
        }
    }
}

#[cfg(test)]
impl Batch {
    pub fn for_test(id: u64) -> Self {
        Self {
            id,
            captured_at_ns: timestamp_ns(),
            entries: HashMap::new(),
        }
    }
    pub fn insert<T: Send + Sync + 'static>(&mut self, key: Key<T>, value: T) {
        self.entries.insert(
            key.name,
            Entry {
                captured_at_ns: self.captured_at_ns,
                result: Ok(Box::new(value)),
            },
        );
    }
}
