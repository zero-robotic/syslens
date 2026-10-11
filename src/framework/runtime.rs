use super::snapshot::Entry;
use super::{Batch, CollectContext, Dispatcher, Key, Sample, timestamp_ns};
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc,
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

struct Worker {
    name: &'static str,
    request: mpsc::SyncSender<()>,
    response: mpsc::Receiver<Entry>,
    handle: JoinHandle<()>,
}

pub struct Runtime {
    collectors: Vec<Worker>,
    assemblers: Vec<Box<dyn Assembler>>,
    dispatcher: Dispatcher,
}
impl Runtime {
    pub fn new(dispatcher: Dispatcher) -> Self {
        Self {
            collectors: Vec::new(),
            assemblers: Vec::new(),
            dispatcher,
        }
    }

    pub fn add_collector<C: Collector>(
        &mut self,
        key: Key<C::Data>,
        mut collector: C,
    ) -> Result<()> {
        ensure!(
            !self.collectors.iter().any(|w| w.name == key.name),
            "duplicate collector {}",
            key.name
        );
        let (request, receiver) = mpsc::sync_channel::<()>(1);
        let (sender, response) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name(format!("collect-{}", key.name))
            .spawn(move || {
                for () in receiver {
                    let context = CollectContext::now();
                    let result = collector
                        .collect(&context)
                        .map(|data| Box::new(data) as Box<dyn std::any::Any + Send + Sync>)
                        .map_err(|error| format!("{error:#}"));
                    if sender
                        .send(Entry {
                            captured_at_ns: context.captured_at_ns,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        self.collectors.push(Worker {
            name: key.name,
            request,
            response,
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
        ensure!(!interval.is_zero(), "interval must be positive");
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
        let mut deadline = Instant::now();
        let mut last_errors = HashMap::new();
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
            let mut batch = Batch {
                id,
                captured_at_ns: timestamp_ns(),
                entries: HashMap::new(),
            };
            for worker in &self.collectors {
                worker
                    .request
                    .send(())
                    .map_err(|_| anyhow!("collector {} stopped", worker.name))?;
            }
            for worker in &self.collectors {
                let entry = worker
                    .response
                    .recv()
                    .map_err(|_| anyhow!("collector {} panicked", worker.name))?;
                if let Err(error) = &entry.result {
                    if last_errors.get(worker.name) != Some(error) {
                        eprintln!("collector {}: {error}", worker.name);
                    }
                    last_errors.insert(worker.name, error.clone());
                } else {
                    last_errors.remove(worker.name);
                }
                batch.entries.insert(worker.name, entry);
            }
            for assembler in &mut self.assemblers {
                for sample in assembler.assemble(&batch)? {
                    self.dispatcher.publish(sample)?;
                }
            }
            deadline += interval;
            if deadline < Instant::now() {
                deadline = Instant::now() + interval;
            }
        }
        Ok(())
    }

    fn stop_collectors(&mut self) -> Result<()> {
        let mut failed = Vec::new();
        for worker in self.collectors.drain(..) {
            drop(worker.request);
            drop(worker.response);
            if worker.handle.join().is_err() {
                failed.push(worker.name);
            }
        }
        ensure!(failed.is_empty(), "collectors panicked: {failed:?}");
        Ok(())
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop_collectors();
    }
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
                Err(_) => {
                    assert!(
                        batch
                            .status(self.key.name)
                            .1
                            .unwrap()
                            .contains("temporary read failure")
                    );
                    0
                }
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
    fn shares_current_batch_handles_failure_and_drains_on_shutdown() {
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
        runtime.add_collector(key, Counter(0)).unwrap();
        assert!(runtime.add_collector(key, Counter(9)).is_err());
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
                Duration::from_millis(1),
                Some(3),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert!(closed.load(Ordering::Relaxed));
        let samples = values.lock().unwrap();
        assert_eq!(samples.len(), 6);
        for (index, expected) in [1, 0, 3].into_iter().enumerate() {
            assert_eq!(samples[index * 2].payload[0], expected);
            assert_eq!(samples[index * 2 + 1].payload[0], expected);
            assert_eq!(
                samples[index * 2].captured_at_ns,
                samples[index * 2 + 1].captured_at_ns
            );
            assert_eq!(samples[index * 2].sequence, index as u32);
        }
    }
}
