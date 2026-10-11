//! Nonblocking fan-out. Each output owns delivery, overload, and failure policy.
use super::{OperationContext, Sample, StreamDescriptor, control::Task};
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

pub trait Output: Send + 'static {
    fn write(&mut self, sample: &Sample, context: &OperationContext) -> Result<()>;
    fn close(&mut self, context: &OperationContext) -> Result<()>;
}
#[derive(Clone, Copy)]
pub enum Delivery {
    All,
    Latest { interval: Duration },
}
#[derive(Clone, Copy)]
pub enum Overflow {
    Disable,
    DropNewest,
    DropOldest,
}
#[derive(Clone, Copy)]
pub enum FailurePolicy {
    Isolate,
    Stop,
}
#[derive(Clone, Copy)]
pub struct OutputOptions {
    pub capacity: usize,
    pub delivery: Delivery,
    pub overflow: Overflow,
    pub failure: FailurePolicy,
    pub operation_timeout: Duration,
}
#[derive(Default)]
struct Pending {
    queue: VecDeque<Arc<Sample>>,
    closing: bool,
    dropped: u64,
    coalesced: u64,
    failure: Option<String>,
}
struct Mailbox {
    pending: Mutex<Pending>,
    wake: Condvar,
    options: OutputOptions,
}
impl Mailbox {
    fn push(&self, sample: Arc<Sample>) {
        let mut pending = self.pending.lock().unwrap();
        if pending.closing {
            return;
        }
        if matches!(self.options.delivery, Delivery::Latest { .. })
            && let Some(existing) = pending
                .queue
                .iter_mut()
                .find(|value| value.stream.topic == sample.stream.topic)
        {
            *existing = sample;
            pending.coalesced += 1;
            self.wake.notify_one();
            return;
        }
        if pending.queue.len() == self.options.capacity {
            pending.dropped += 1;
            match self.options.overflow {
                Overflow::Disable => {
                    pending.closing = true;
                    pending.failure = Some(
                        "queue full; output disabled, accepted messages will be drained".into(),
                    );
                    self.wake.notify_one();
                    return;
                }
                Overflow::DropNewest => return,
                Overflow::DropOldest => {
                    pending.queue.pop_front();
                }
            }
        }
        pending.queue.push_back(sample);
        self.wake.notify_one();
    }
    fn next(
        &self,
        last: &HashMap<String, Instant>,
        context: &super::control::Control,
    ) -> Option<Arc<Sample>> {
        let mut pending = self.pending.lock().unwrap();
        loop {
            if context.cancel.is_cancelled() {
                return None;
            }
            let ready = pending.queue.iter().position(|sample| {
                pending.closing
                    || match self.options.delivery {
                        Delivery::All => true,
                        Delivery::Latest { interval } => last
                            .get(&sample.stream.topic)
                            .is_none_or(|time| time.elapsed() >= interval),
                    }
            });
            if let Some(index) = ready {
                return pending.queue.remove(index);
            }
            if pending.closing && pending.queue.is_empty() {
                return None;
            }
            pending = self
                .wake
                .wait_timeout(pending, Duration::from_millis(5))
                .unwrap()
                .0;
        }
    }
    fn close(&self) {
        self.pending.lock().unwrap().closing = true;
        self.wake.notify_all();
    }
}
struct Worker {
    name: String,
    mailbox: Arc<Mailbox>,
    task: Task,
    failure: Option<String>,
    reported_drops: u64,
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
        options: OutputOptions,
    ) -> Result<()> {
        ensure!(
            options.capacity > 0 && !options.operation_timeout.is_zero(),
            "invalid output limits"
        );
        if let Delivery::Latest { interval } = options.delivery {
            ensure!(!interval.is_zero(), "live interval must be positive");
        }
        ensure!(
            !self.workers.iter().any(|worker| worker.name == name),
            "duplicate output {name}"
        );
        let mailbox = Arc::new(Mailbox {
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            options,
        });
        let inbox = mailbox.clone();
        let task = Task::spawn(format!("output-{name}"), move |control| {
            let mut last = HashMap::new();
            let result = (|| {
                while let Some(sample) = inbox.next(&last, &control) {
                    let operation = control.operation(options.operation_timeout);
                    operation.context.check()?;
                    output.write(&sample, &operation.context)?;
                    operation.context.check()?;
                    last.insert(sample.stream.topic.clone(), Instant::now());
                }
                Ok(())
            })();
            inbox.close();
            // close has its own bound, even after write fails or is cancelled.
            let _close_operation = control.operation(options.operation_timeout);
            let close_context =
                OperationContext::new(super::Cancellation::default(), options.operation_timeout);
            let closed = output
                .close(&close_context)
                .and_then(|_| close_context.check());
            result.and(closed)
        })?;
        self.workers.push(Worker {
            name: name.into(),
            mailbox,
            task,
            failure: None,
            reported_drops: 0,
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
        for worker in &self.workers {
            worker.mailbox.push(sample.clone());
        }
        self.poll()
    }
    pub fn poll(&mut self) -> Result<()> {
        for worker in &mut self.workers {
            let error = worker
                .task
                .poll()
                .map(str::to_string)
                .or_else(|| worker.mailbox.pending.lock().unwrap().failure.clone());
            if worker.failure.is_none()
                && let Some(error) = error
            {
                eprintln!("output {}: {error}", worker.name);
                worker.mailbox.close();
                worker.failure = Some(error);
            }
            let dropped = worker.mailbox.pending.lock().unwrap().dropped;
            if dropped > worker.reported_drops
                && (worker.reported_drops == 0 || dropped / 100 > worker.reported_drops / 100)
            {
                eprintln!("output {}: {dropped} messages dropped", worker.name);
                worker.reported_drops = dropped;
            }
            if matches!(worker.mailbox.options.failure, FailurePolicy::Stop)
                && let Some(error) = &worker.failure
            {
                return Err(anyhow!("output {}: {error}", worker.name));
            }
        }
        ensure!(
            self.workers.is_empty() || self.workers.iter().any(|worker| worker.failure.is_none()),
            "all outputs have failed"
        );
        Ok(())
    }
    pub fn finish(&mut self, deadline: Instant) -> Result<()> {
        for worker in &self.workers {
            worker.mailbox.close();
        }
        let mut errors = Vec::new();
        for mut worker in self.workers.drain(..) {
            if let Err(error) = worker.task.finish(deadline) {
                errors.push(format!("{error:#}"));
            }
            let pending = worker.mailbox.pending.lock().unwrap();
            if let Some(error) = worker.failure.as_ref().or(pending.failure.as_ref()) {
                errors.push(format!("{}: {error}", worker.name));
            }
            if pending.dropped > 0 {
                eprintln!(
                    "output {}: {} messages dropped",
                    worker.name, pending.dropped
                );
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
}
impl Drop for Dispatcher {
    fn drop(&mut self) {
        for worker in &self.workers {
            worker.mailbox.close();
            worker.task.control.cancel.cancel();
        }
    }
}
