use anyhow::{Context, Result, anyhow};
use std::{
    any::Any,
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
    pub started: Instant,
    pub interval: Duration,
    pub max_age: Duration,
    pub actual_interval: Option<Duration>,
    pub duration: Duration,
    pub sequence: u64,
    pub missed_periods: u64,
    pub result: Result<Box<dyn Any + Send + Sync>, String>,
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
    pub skipped_samples: u64,
}

pub struct Batch {
    pub id: u64,
    pub captured_at_ns: u64,
    pub(super) observed_at: Instant,
    pub(super) entries: HashMap<&'static str, Arc<Entry>>,
    pub(super) skipped_samples: HashMap<&'static str, u64>,
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
        if self.observed_at.saturating_duration_since(entry.started) > entry.max_age {
            return Err(anyhow!("stale input {}", key.name));
        }
        data.downcast_ref::<T>()
            .with_context(|| format!("input type mismatch for {}", key.name))
    }

    pub fn status(&self, name: &str) -> InputStatus {
        let entry = self.entries.get(name);
        let sampled = entry.filter(|entry| entry.sequence > 0);
        let age = sampled.map(|entry| self.observed_at.saturating_duration_since(entry.started));
        let stale = sampled.is_some_and(|entry| age.unwrap() > entry.max_age);
        let error = match entry {
            None => Some(format!("missing input {name}")),
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
            collection_duration_ms: sampled.map(|entry| milliseconds(entry.duration)),
            age_ms: age.map(milliseconds),
            stale,
            sample_sequence: entry.map_or(0, |entry| entry.sequence),
            missed_periods: entry.map_or(0, |entry| entry.missed_periods),
            skipped_samples: self.skipped_samples.get(name).copied().unwrap_or(0),
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
            observed_at: Instant::now(),
            entries: HashMap::new(),
            skipped_samples: HashMap::new(),
        }
    }
    pub fn insert<T: Send + Sync + 'static>(&mut self, key: Key<T>, value: T) {
        self.entries.insert(
            key.name,
            Arc::new(Entry {
                captured_at_ns: self.captured_at_ns,
                started: self.observed_at,
                interval: Duration::from_millis(100),
                max_age: Duration::from_millis(300),
                actual_interval: None,
                duration: Duration::ZERO,
                sequence: 1,
                missed_periods: 0,
                result: Ok(Box::new(value)),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_data_expires_using_monotonic_age_and_failures_replace_successes() {
        let key = Key::new("sample");
        let mut batch = Batch::for_test(0);
        batch.insert(key, 7u8);
        batch.observed_at += Duration::from_millis(200);
        assert_eq!(*batch.get(key).unwrap(), 7);
        assert_eq!(batch.status("sample").age_ms, Some(200.0));
        batch.observed_at += Duration::from_millis(101);
        assert!(batch.get(key).is_err());
        assert!(batch.status("sample").stale);
        let entry = Arc::get_mut(batch.entries.get_mut("sample").unwrap()).unwrap();
        entry.started = batch.observed_at;
        entry.result = Err("read failed".into());
        assert!(batch.get(key).is_err());
        assert_eq!(batch.status("sample").error.as_deref(), Some("read failed"));
        assert!(!batch.status("sample").stale);
    }
}
