use super::snapshot::Entry;
use super::{Batch, CollectContext, Dispatcher, Key, Sample, timestamp_ns};
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub trait Collector: Send + 'static {
    type Data: Send + Sync + 'static;
    fn collect(&mut self, context: &CollectContext) -> Result<Self::Data>;
}
pub trait Assembler: Send {
    fn name(&self) -> &str;
    fn dependencies(&self) -> &[&'static str];
    fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>>;
}

#[derive(Clone, Copy)]
pub struct Schedule {
    interval: Duration,
    max_age: Duration,
}
impl Schedule {
    pub fn new(interval: Duration, stale_after_periods: u32) -> Result<Self> {
        ensure!(
            !interval.is_zero() && interval <= Duration::from_secs(86400),
            "collection interval must be positive and at most 24 hours"
        );
        ensure!(
            stale_after_periods > 0,
            "stale_after_periods must be positive"
        );
        let max_age = interval
            .checked_mul(stale_after_periods)
            .ok_or_else(|| anyhow::anyhow!("maximum sample age overflow"))?;
        Ok(Self { interval, max_age })
    }
}

struct Worker {
    name: &'static str,
    start: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}
type Latest = Arc<Mutex<HashMap<&'static str, Arc<Entry>>>>;

pub struct Runtime {
    collectors: Vec<Worker>,
    latest: Latest,
    assemblers: Vec<Box<dyn Assembler>>,
    dispatcher: Dispatcher,
}
impl Runtime {
    pub fn new(dispatcher: Dispatcher) -> Self {
        Self {
            collectors: Vec::new(),
            latest: Arc::new(Mutex::new(HashMap::new())),
            assemblers: Vec::new(),
            dispatcher,
        }
    }
    pub fn add_collector<C: Collector>(
        &mut self,
        key: Key<C::Data>,
        mut collector: C,
        schedule: Schedule,
    ) -> Result<()> {
        ensure!(
            !self.collectors.iter().any(|w| w.name == key.name),
            "duplicate collector {}",
            key.name
        );
        let (start, control) = mpsc::channel();
        let latest = self.latest.clone();
        let handle = thread::Builder::new()
            .name(format!("collect-{}", key.name))
            .spawn(move || {
                // Registration does not collect while other sources are being probed.
                if control.recv().is_err() {
                    return;
                }
                let mut deadline = Instant::now();
                let mut previous_start = None;
                let mut sequence = 0;
                let mut missed_periods = 0;
                let mut last_error = None;
                loop {
                    let started = Instant::now();
                    let context = CollectContext::now();
                    let result = collector
                        .collect(&context)
                        .map(|data| Box::new(data) as Box<dyn std::any::Any + Send + Sync>)
                        .map_err(|error| format!("{error:#}"));
                    let finished = Instant::now();
                    let (next, skipped) = next_deadline(deadline, schedule.interval, finished);
                    missed_periods += skipped;
                    sequence += 1;
                    if let Err(error) = &result {
                        if last_error.as_ref() != Some(error) {
                            eprintln!("collector {}: {error}", key.name);
                        }
                        last_error = Some(error.clone());
                    } else {
                        last_error = None;
                    }
                    let entry = Entry {
                        captured_at_ns: context.captured_at_ns,
                        started,
                        interval: schedule.interval,
                        max_age: schedule.max_age,
                        actual_interval: previous_start
                            .map(|previous| started.duration_since(previous)),
                        duration: finished.duration_since(started),
                        sequence,
                        missed_periods,
                        result,
                    };
                    // Collection never holds this lock. Only a pointer is replaced.
                    latest.lock().unwrap().insert(key.name, Arc::new(entry));
                    previous_start = Some(started);
                    deadline = next;
                    match control.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            })?;
        self.latest.lock().unwrap().insert(
            key.name,
            Arc::new(Entry {
                captured_at_ns: 0,
                started: Instant::now(),
                interval: schedule.interval,
                max_age: schedule.max_age,
                actual_interval: None,
                duration: Duration::ZERO,
                sequence: 0,
                missed_periods: 0,
                result: Err("waiting for first collection".into()),
            }),
        );
        self.collectors.push(Worker {
            name: key.name,
            start,
            handle,
        });
        Ok(())
    }
    pub fn add_assembler(&mut self, assembler: impl Assembler + 'static) -> Result<()> {
        for dependency in assembler.dependencies() {
            ensure!(
                self.collectors
                    .iter()
                    .any(|worker| worker.name == *dependency),
                "assembler {} requires unregistered collector {dependency}",
                assembler.name()
            );
        }
        self.assemblers.push(Box::new(assembler));
        Ok(())
    }
    pub fn run(
        &mut self,
        interval: Duration,
        limit: Option<u64>,
        stop: Arc<AtomicBool>,
    ) -> Result<()> {
        ensure!(
            !interval.is_zero() && interval <= Duration::from_secs(86400),
            "publication interval must be positive and at most 24 hours"
        );
        let result = self.run_loop(interval, limit, &stop);
        let collectors = self.stop_collectors();
        let outputs = self.dispatcher.finish();
        let failures: Vec<_> = [result, collectors, outputs]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect();
        ensure!(failures.is_empty(), "{}", failures.join("; "));
        Ok(())
    }
    fn run_loop(
        &mut self,
        interval: Duration,
        limit: Option<u64>,
        stop: &AtomicBool,
    ) -> Result<()> {
        for worker in &self.collectors {
            worker.start.send(())?;
        }
        let mut deadline = Instant::now() + interval;
        let mut previous_sequences = HashMap::new();
        let mut skipped_samples = HashMap::<&'static str, u64>::new();
        for id in 0..limit.unwrap_or(u64::MAX) {
            while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50)),
                );
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            for worker in &self.collectors {
                ensure!(
                    !worker.handle.is_finished(),
                    "collector {} stopped unexpectedly",
                    worker.name
                );
            }
            let entries = self.latest.lock().unwrap().clone();
            for (&name, entry) in &entries {
                let previous = previous_sequences.insert(name, entry.sequence).unwrap_or(0);
                *skipped_samples.entry(name).or_default() +=
                    entry.sequence.saturating_sub(previous).saturating_sub(1);
            }
            let batch = Batch {
                id,
                captured_at_ns: timestamp_ns(),
                observed_at: Instant::now(),
                entries,
                skipped_samples: skipped_samples.clone(),
            };
            for assembler in &mut self.assemblers {
                for sample in assembler.assemble(&batch)? {
                    self.dispatcher.publish(sample)?;
                }
            }
            deadline = next_deadline(deadline, interval, Instant::now()).0;
        }
        Ok(())
    }
    fn stop_collectors(&mut self) -> Result<()> {
        let handles: Vec<_> = self
            .collectors
            .drain(..)
            .map(|worker| {
                drop(worker.start); // Interrupt all sleeping collectors before joining any.
                (worker.name, worker.handle)
            })
            .collect();
        let failed: Vec<_> = handles
            .into_iter()
            .filter_map(|(name, handle)| handle.join().err().map(|_| name))
            .collect();
        ensure!(failed.is_empty(), "collectors panicked: {failed:?}");
        Ok(())
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop_collectors();
    }
}

// Advance on the original monotonic grid, skipping missed slots without catch-up
// bursts or adding a full extra period after a slow collection.
fn next_deadline(previous: Instant, interval: Duration, finished: Instant) -> (Instant, u64) {
    let next = previous + interval;
    if next > finished {
        return (next, 0);
    }
    let late = finished.duration_since(next).as_nanos();
    let skipped = (late / interval.as_nanos() + 1) as u64;
    let remainder = Duration::from_nanos((late % interval.as_nanos()) as u64);
    (finished + (interval - remainder), skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::{Output, Overflow, StreamDescriptor};
    use bytes::Bytes;
    use std::sync::Mutex;

    struct Counter(u8);
    impl Collector for Counter {
        type Data = u8;
        fn collect(&mut self, _: &CollectContext) -> Result<u8> {
            self.0 += 1;
            ensure!(self.0 != 2, "temporary read failure");
            Ok(self.0)
        }
    }
    struct Combiner {
        key: Key<u8>,
        dependencies: [&'static str; 1],
        topic: &'static str,
    }
    impl Assembler for Combiner {
        fn name(&self) -> &str {
            self.topic
        }
        fn dependencies(&self) -> &[&'static str] {
            &self.dependencies
        }
        fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>> {
            let value = match batch.get(self.key) {
                Ok(value) => *value,
                Err(_) => 0,
            };
            Ok(vec![Sample {
                stream: Arc::new(StreamDescriptor {
                    topic: self.topic.into(),
                    message_encoding: "test".into(),
                    schema_name: "test".into(),
                    schema_encoding: "test".into(),
                    schema: Bytes::new(),
                }),
                captured_at_ns: batch.captured_at_ns,
                sequence: 0,
                payload: Bytes::from(vec![value]),
            }])
        }
    }
    struct Capture(Arc<Mutex<Vec<Sample>>>, Arc<AtomicBool>);
    impl Output for Capture {
        fn write(&mut self, sample: &Sample) -> Result<()> {
            self.0.lock().unwrap().push(sample.clone());
            Ok(())
        }
        fn close(&mut self) -> Result<()> {
            self.1.store(true, Ordering::Relaxed);
            Ok(())
        }
    }
    #[test]
    fn shares_latest_snapshot_and_drains_on_shutdown() {
        let values = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let mut dispatcher = Dispatcher::default();
        dispatcher
            .add(
                "capture",
                Capture(values.clone(), closed.clone()),
                1,
                Overflow::Block,
            )
            .unwrap();
        let mut runtime = Runtime::new(dispatcher);
        let key = Key::new("counter");
        assert!(
            runtime
                .add_assembler(Combiner {
                    key,
                    dependencies: [key.name],
                    topic: "/missing"
                })
                .is_err()
        );
        runtime
            .add_collector(
                key,
                Counter(0),
                Schedule::new(Duration::from_millis(5), 3).unwrap(),
            )
            .unwrap();
        assert!(
            runtime
                .add_collector(
                    key,
                    Counter(9),
                    Schedule::new(Duration::from_millis(5), 3).unwrap()
                )
                .is_err()
        );
        for topic in ["/first", "/second"] {
            runtime
                .add_assembler(Combiner {
                    key,
                    dependencies: [key.name],
                    topic,
                })
                .unwrap();
        }
        runtime
            .run(
                Duration::from_millis(10),
                Some(3),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert!(closed.load(Ordering::Relaxed));
        let samples = values.lock().unwrap();
        assert_eq!(samples.len(), 6);
        assert!(samples.iter().any(|sample| sample.payload[0] > 0));
        for index in 0..3 {
            assert_eq!(samples[index * 2].payload, samples[index * 2 + 1].payload);
            assert_eq!(
                samples[index * 2].captured_at_ns,
                samples[index * 2 + 1].captured_at_ns
            );
            assert_eq!(samples[index * 2].sequence, index as u32);
        }
    }

    #[test]
    fn slow_collection_does_not_block_fast_collection_or_publication() {
        struct Fast(u8);
        impl Collector for Fast {
            type Data = u8;
            fn collect(&mut self, _: &CollectContext) -> Result<u8> {
                self.0 = self.0.saturating_add(1);
                Ok(self.0)
            }
        }
        struct Slow(mpsc::Sender<()>, mpsc::Receiver<()>);
        impl Collector for Slow {
            type Data = u8;
            fn collect(&mut self, _: &CollectContext) -> Result<u8> {
                self.0.send(())?;
                self.1.recv_timeout(Duration::from_secs(5))?;
                Ok(1)
            }
        }
        struct Notice(mpsc::Sender<Vec<u8>>);
        impl Output for Notice {
            fn write(&mut self, sample: &Sample) -> Result<()> {
                self.0.send(sample.payload.to_vec())?;
                Ok(())
            }
            fn close(&mut self) -> Result<()> {
                Ok(())
            }
        }
        let (entered, ready) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (notice, samples) = mpsc::channel();
        let mut dispatcher = Dispatcher::default();
        dispatcher
            .add("notice", Notice(notice), 128, Overflow::Block)
            .unwrap();
        let mut runtime = Runtime::new(dispatcher);
        let key = Key::new("fast");
        runtime
            .add_collector(
                key,
                Fast(0),
                Schedule::new(Duration::from_millis(5), 100).unwrap(),
            )
            .unwrap();
        runtime
            .add_collector(
                Key::new("slow"),
                Slow(entered, blocked),
                Schedule::new(Duration::from_secs(10), 3).unwrap(),
            )
            .unwrap();
        runtime
            .add_assembler(Combiner {
                key,
                dependencies: [key.name],
                topic: "/fast",
            })
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let worker = thread::spawn(move || runtime.run(Duration::from_millis(5), None, signal));
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        // The slow source stays blocked until a third fast observation is published.
        let deadline = Instant::now() + Duration::from_secs(2);
        let progressed = loop {
            match samples.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(sample) if sample[0] >= 3 => break true,
                Ok(_) => {}
                Err(_) => break false,
            }
        };
        stop.store(true, Ordering::Relaxed);
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert!(progressed, "a slow source blocked unrelated samples");
    }

    #[test]
    fn overruns_skip_slots_without_drifting_or_bursting() {
        let start = Instant::now();
        let interval = Duration::from_millis(100);
        let (next, missed) = next_deadline(start, interval, start + Duration::from_millis(250));
        assert_eq!(next, start + Duration::from_millis(300));
        assert_eq!(missed, 2);
        assert_eq!(
            next_deadline(start, interval, start + Duration::from_millis(20)),
            (start + interval, 0)
        );
        assert!(Schedule::new(Duration::ZERO, 3).is_err());
        assert!(Schedule::new(interval, 0).is_err());
    }
}
