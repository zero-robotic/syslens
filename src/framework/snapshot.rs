use anyhow::{Result, anyhow, ensure};
use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    marker::PhantomData,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub fn timestamp_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub name: &'static str,
    pub(super) type_id: TypeId,
    pub(super) type_name: &'static str,
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
impl<T: 'static> Key<T> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }
    pub fn dependency(self) -> Dependency {
        Dependency {
            name: self.name,
            type_id: TypeId::of::<T>(),
            type_name: type_name::<T>(),
        }
    }
}
pub(super) struct Entry {
    pub input: Dependency,
    pub captured_at_ns: u64,
    pub started: Instant,
    pub completed: Instant,
    pub interval: Duration,
    pub max_age: Duration,
    pub actual_interval: Option<Duration>,
    pub sequence: u64,
    pub missed_periods: u64,
    pub result: Result<Box<dyn Any + Send + Sync>, String>,
}
impl Entry {
    pub fn unavailable(input: Dependency, error: String) -> Self {
        Self {
            input,
            captured_at_ns: 0,
            started: Instant::now(),
            completed: Instant::now(),
            interval: Duration::ZERO,
            max_age: Duration::MAX,
            actual_interval: None,
            sequence: 0,
            missed_periods: 0,
            result: Err(error),
        }
    }
}
pub struct InputStatus {
    pub captured_at_ns: Option<u64>,
    pub error: Option<String>,
    pub interval_ms: u64,
    pub actual_interval_ms: Option<f64>,
    pub collection_duration_ms: Option<f64>,
    pub age_ms: Option<f64>,
    pub stale: bool,
    pub sample_sequence: u64,
    pub missed_periods: u64,
}
/// One observation triggers a view of its declared dependencies. Retained inputs
/// are evaluated as of this observation, not as of a later output/flush time.
pub struct Batch {
    pub id: u64,
    pub captured_at_ns: u64,
    pub trigger: &'static str,
    pub(super) observed_at: Instant,
    pub(super) entries: HashMap<&'static str, Arc<Entry>>,
}
impl Batch {
    /// Unavailable/stale measurements are expected; wiring/type errors are not.
    pub fn read<T: Send + Sync + 'static>(&self, key: Key<T>) -> Result<Option<&T>> {
        let entry = self
            .entries
            .get(key.name)
            .ok_or_else(|| anyhow!("undeclared input {}", key.name))?;
        ensure!(
            entry.input.type_id == TypeId::of::<T>(),
            "input type mismatch for {}: registered {}, requested {}",
            key.name,
            entry.input.type_name,
            type_name::<T>()
        );
        if key.name != self.trigger
            && self.observed_at.saturating_duration_since(entry.completed) > entry.max_age
        {
            return Ok(None);
        }
        match &entry.result {
            Ok(data) => data
                .downcast_ref::<T>()
                .map(Some)
                .ok_or_else(|| anyhow!("corrupt input type for {}", key.name)),
            Err(_) => Ok(None),
        }
    }
    pub fn status(&self, name: &str) -> InputStatus {
        let entry = self.entries.get(name);
        let sampled = entry.filter(|entry| entry.sequence > 0);
        let age = sampled.map(|entry| self.observed_at.saturating_duration_since(entry.completed));
        let stale =
            name != self.trigger && sampled.is_some_and(|entry| age.unwrap() > entry.max_age);
        let error = match entry {
            None => Some(format!("undeclared input {name}")),
            Some(entry) => entry
                .result
                .as_ref()
                .err()
                .cloned()
                .or_else(|| stale.then(|| format!("stale input {name}"))),
        };
        InputStatus {
            captured_at_ns: sampled.map(|entry| entry.captured_at_ns),
            error,
            interval_ms: entry.map_or(0, |entry| entry.interval.as_millis() as u64),
            actual_interval_ms: sampled
                .and_then(|entry| entry.actual_interval)
                .map(milliseconds),
            collection_duration_ms: sampled
                .map(|entry| milliseconds(entry.completed.duration_since(entry.started))),
            age_ms: age.map(milliseconds),
            stale,
            sample_sequence: entry.map_or(0, |entry| entry.sequence),
            missed_periods: entry.map_or(0, |entry| entry.missed_periods),
        }
    }
}
fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[cfg(test)]
impl Batch {
    pub fn for_test(id: u64) -> Self {
        Self {
            id,
            captured_at_ns: timestamp_ns(),
            trigger: "",
            observed_at: Instant::now(),
            entries: HashMap::new(),
        }
    }
    pub fn insert<T: Send + Sync + 'static>(&mut self, key: Key<T>, value: T) {
        self.entries.insert(
            key.name,
            Arc::new(Entry {
                input: key.dependency(),
                captured_at_ns: self.captured_at_ns,
                started: self.observed_at,
                completed: self.observed_at,
                interval: Duration::from_millis(100),
                max_age: Duration::from_millis(300),
                actual_interval: None,
                sequence: 1,
                missed_periods: 0,
                result: Ok(Box::new(value)),
            }),
        );
    }
    pub fn unavailable<T: 'static>(&mut self, key: Key<T>, reason: &str) {
        self.entries.insert(
            key.name,
            Arc::new(Entry::unavailable(key.dependency(), reason.into())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_trigger_is_preserved_and_old_retained_inputs_expire() {
        let key = Key::new("sample");
        let mut batch = Batch::for_test(0);
        batch.insert(key, 7_u64);
        batch.observed_at += Duration::from_secs(1);
        assert!(batch.read(key).unwrap().is_none());
        assert!(batch.status(key.name).stale);
        batch.trigger = key.name;
        assert_eq!(batch.read(key).unwrap(), Some(&7));
        assert!(!batch.status(key.name).stale);
        batch.unavailable(key, "read failed");
        assert!(batch.read(key).unwrap().is_none());
        assert_eq!(batch.status(key.name).error.as_deref(), Some("read failed"));
    }
}
