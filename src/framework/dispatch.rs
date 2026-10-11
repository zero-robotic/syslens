use super::{Sample, StreamDescriptor};
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

pub trait Output: Send + 'static {
    fn write(&mut self, sample: &Sample) -> Result<()>;
    fn close(&mut self) -> Result<()>;
}

#[derive(Clone, Copy)]
pub enum Overflow {
    Block,
    DropNewest,
}

struct Worker {
    name: String,
    sender: SyncSender<Arc<Sample>>,
    handle: JoinHandle<Result<()>>,
    policy: Overflow,
    dropped: u64,
}

#[derive(Default)]
pub struct Dispatcher {
    workers: Vec<Worker>,
    streams: HashMap<String, (Arc<StreamDescriptor>, u32)>,
}
impl Dispatcher {
    pub fn add(
        &mut self,
        name: &str,
        mut output: impl Output,
        capacity: usize,
        policy: Overflow,
    ) -> Result<()> {
        ensure!(capacity > 0, "output queue must not be empty");
        let (sender, receiver) = mpsc::sync_channel::<Arc<Sample>>(capacity);
        let handle = thread::Builder::new()
            .name(format!("output-{name}"))
            .spawn(move || {
                let result = (|| {
                    for sample in receiver {
                        output.write(&sample)?;
                    }
                    Ok(())
                })();
                let close = output.close();
                result.and(close)
            })?;
        self.workers.push(Worker {
            name: name.into(),
            sender,
            handle,
            policy,
            dropped: 0,
        });
        Ok(())
    }

    pub fn publish(&mut self, mut sample: Sample) -> Result<()> {
        let entry = self
            .streams
            .entry(sample.stream.topic.clone())
            .or_insert_with(|| (sample.stream.clone(), 0));
        ensure!(
            entry.0 == sample.stream,
            "conflicting schema for topic {}",
            sample.stream.topic
        );
        sample.sequence = entry.1;
        entry.1 = entry.1.wrapping_add(1);
        let sample = Arc::new(sample);
        // Best-effort outputs are serviced before any lossless output applies backpressure.
        for best_effort in [true, false] {
            for worker in &mut self.workers {
                if matches!(worker.policy, Overflow::DropNewest) != best_effort {
                    continue;
                }
                match worker.policy {
                    Overflow::Block => worker
                        .sender
                        .send(sample.clone())
                        .map_err(|_| anyhow!("output {} failed", worker.name))?,
                    Overflow::DropNewest => match worker.sender.try_send(sample.clone()) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => worker.dropped += 1,
                        Err(TrySendError::Disconnected(_)) => {
                            return Err(anyhow!("output {} failed", worker.name));
                        }
                    },
                }
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        for worker in self.workers.drain(..) {
            drop(worker.sender);
            if worker.dropped > 0 {
                eprintln!(
                    "{}: skipped {} live messages (queue full)",
                    worker.name, worker.dropped
                );
            }
            match worker.handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("{}: {error:#}", worker.name)),
                Err(_) => failures.push(format!("{}: worker panicked", worker.name)),
            }
        }
        ensure!(failures.is_empty(), "{}", failures.join("; "));
        Ok(())
    }
}
impl Drop for Dispatcher {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    fn sample() -> Sample {
        Sample {
            stream: Arc::new(StreamDescriptor {
                topic: "/test".into(),
                message_encoding: "test".into(),
                schema_name: "test".into(),
                schema_encoding: "test".into(),
                schema: Bytes::new(),
            }),
            captured_at_ns: 1,
            sequence: 0,
            payload: Bytes::from_static(b"data"),
        }
    }
    struct SlowOutput {
        first: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
        samples: Arc<Mutex<Vec<Sample>>>,
    }
    impl Output for SlowOutput {
        fn write(&mut self, sample: &Sample) -> Result<()> {
            if let Some((entered, release)) = self.first.take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            self.samples.lock().unwrap().push(sample.clone());
            Ok(())
        }
        fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn bounded_live_queue_drops_without_losing_recorded_messages() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let live = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher = Dispatcher::default();
        dispatcher
            .add(
                "live",
                SlowOutput {
                    first: Some((entered_tx, release_rx)),
                    samples: live.clone(),
                },
                1,
                Overflow::DropNewest,
            )
            .unwrap();
        dispatcher
            .add(
                "record",
                SlowOutput {
                    first: None,
                    samples: recording.clone(),
                },
                1,
                Overflow::Block,
            )
            .unwrap();
        dispatcher.publish(sample()).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        dispatcher.publish(sample()).unwrap();
        dispatcher.publish(sample()).unwrap();
        assert_eq!(dispatcher.workers[0].dropped, 1);
        let mut conflict = sample();
        Arc::get_mut(&mut conflict.stream).unwrap().schema_name = "another".into();
        assert!(dispatcher.publish(conflict).is_err());
        release_tx.send(()).unwrap();
        dispatcher.finish().unwrap();
        let live = live.lock().unwrap();
        let recording = recording.lock().unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(recording.len(), 3);
        for index in 0..2 {
            // Both outputs receive the same allocation, not separately encoded payloads.
            assert_eq!(
                live[index].payload.as_ptr(),
                recording[index].payload.as_ptr()
            );
        }
        assert_eq!(
            recording
                .iter()
                .map(|sample| sample.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
    struct FailedOutput(Arc<AtomicBool>);
    impl Output for FailedOutput {
        fn write(&mut self, _: &Sample) -> Result<()> {
            anyhow::bail!("disk full")
        }
        fn close(&mut self) -> Result<()> {
            self.0.store(true, Ordering::Relaxed);
            Ok(())
        }
    }
    #[test]
    fn output_failure_is_reported_and_close_still_runs() {
        let closed = Arc::new(AtomicBool::new(false));
        let mut dispatcher = Dispatcher::default();
        dispatcher
            .add("record", FailedOutput(closed.clone()), 1, Overflow::Block)
            .unwrap();
        dispatcher.publish(sample()).unwrap();
        assert!(
            dispatcher
                .finish()
                .unwrap_err()
                .to_string()
                .contains("disk full")
        );
        assert!(closed.load(Ordering::Relaxed));
    }
}
