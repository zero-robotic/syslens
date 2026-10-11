use super::{
    Batch, Dependency, Dispatcher, Key, OperationContext, Sample, control::Task, snapshot::Entry,
    timestamp_ns,
};
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub trait Collector: Send + 'static {
    type Data: Send + Sync + 'static;
    fn collect(&mut self, context: &OperationContext) -> Result<Self::Data>;
}
impl<T: Send + Sync + 'static> Collector for Box<dyn Collector<Data = T>> {
    type Data = T;
    fn collect(&mut self, context: &OperationContext) -> Result<T> {
        (**self).collect(context)
    }
}
pub trait Assembler: Send {
    fn name(&self) -> &str;
    fn dependencies(&self) -> &[Dependency];
    fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>>;
}
#[derive(Clone, Copy)]
pub struct Schedule {
    interval: Duration,
    max_age: Duration,
    timeout: Duration,
}
impl Schedule {
    pub fn new(interval: Duration, stale_after_periods: u32, timeout: Duration) -> Result<Self> {
        ensure!(
            !interval.is_zero() && interval <= Duration::from_secs(86400),
            "invalid collection interval"
        );
        ensure!(
            stale_after_periods > 0 && !timeout.is_zero(),
            "invalid collection timeout/expiry"
        );
        Ok(Self {
            interval,
            max_age: interval
                .checked_mul(stale_after_periods)
                .ok_or_else(|| anyhow!("maximum sample age overflow"))?,
            timeout,
        })
    }
}
#[derive(Clone, Copy)]
pub struct RuntimeOptions {
    pub ingress_capacity: usize,
    pub ingress_timeout: Duration,
    pub shutdown_timeout: Duration,
}
struct Worker {
    input: Dependency,
    start: mpsc::Sender<()>,
    task: Task,
    failure: Option<String>,
}
pub struct Runtime {
    collectors: Vec<Worker>,
    entries: HashMap<&'static str, Arc<Entry>>,
    assemblers: Vec<Box<dyn Assembler>>,
    dispatcher: Dispatcher,
    options: RuntimeOptions,
    sender: mpsc::SyncSender<Entry>,
    receiver: mpsc::Receiver<Entry>,
    batch_id: u64,
    started: bool,
}
impl Runtime {
    pub fn new(dispatcher: Dispatcher, options: RuntimeOptions) -> Result<Self> {
        ensure!(
            options.ingress_capacity > 0
                && !options.ingress_timeout.is_zero()
                && !options.shutdown_timeout.is_zero(),
            "invalid runtime limits"
        );
        let (sender, receiver) = mpsc::sync_channel(options.ingress_capacity);
        Ok(Self {
            collectors: Vec::new(),
            entries: HashMap::new(),
            assemblers: Vec::new(),
            dispatcher,
            options,
            sender,
            receiver,
            batch_id: 0,
            started: false,
        })
    }
    pub fn add_unavailable<T: Send + Sync + 'static>(
        &mut self,
        key: Key<T>,
        error: String,
    ) -> Result<()> {
        ensure!(
            !self.entries.contains_key(key.name),
            "duplicate input {}",
            key.name
        );
        self.entries.insert(
            key.name,
            Arc::new(Entry::unavailable(key.dependency(), error)),
        );
        Ok(())
    }
    /// A capability is registered once, including a typed unavailable entry.
    pub fn add_capability<T: Send + Sync + 'static>(
        &mut self,
        key: Key<T>,
        collector: Result<Box<dyn Collector<Data = T>>, String>,
        schedule: Schedule,
    ) -> Result<()> {
        match collector {
            Ok(collector) => self.add_collector(key, collector, schedule),
            Err(error) => {
                eprintln!("input {} unavailable for this run: {error}", key.name);
                self.add_unavailable(key, error)
            }
        }
    }
    pub fn add_collector<C: Collector>(
        &mut self,
        key: Key<C::Data>,
        mut collector: C,
        schedule: Schedule,
    ) -> Result<()> {
        self.add_unavailable(key, "waiting for first observation".into())?;
        let initial = Arc::get_mut(self.entries.get_mut(key.name).unwrap()).unwrap();
        initial.interval = schedule.interval;
        initial.max_age = schedule.max_age;
        let (start, control_rx) = mpsc::channel();
        let sender = self.sender.clone();
        let input = key.dependency();
        let ingress_timeout = self.options.ingress_timeout;
        let task = Task::spawn(format!("collect-{}", key.name), move |control| {
            if control_rx.recv().is_err() {
                return Ok(());
            }
            let mut deadline = Instant::now();
            let mut previous_start = None;
            let mut sequence = 0;
            let mut missed_periods = 0;
            let mut last_error = None;
            while !control.cancel.is_cancelled() {
                let started = Instant::now();
                let operation = control.operation(schedule.timeout);
                let result = collector
                    .collect(&operation.context)
                    .and_then(|data| {
                        operation.context.check_deadline()?;
                        Ok(data)
                    })
                    .map(|data| Box::new(data) as Box<dyn std::any::Any + Send + Sync>)
                    .map_err(|error| format!("{error:#}"));
                let expired = operation.context.remaining().is_zero();
                let captured_at_ns = operation.context.captured_at_ns;
                drop(operation);
                let completed = Instant::now();
                let (next, skipped) = next_deadline(deadline, schedule.interval, completed);
                missed_periods += skipped;
                sequence += 1;
                if let Err(error) = &result {
                    if last_error.as_ref() != Some(error) {
                        eprintln!("collector {}: {error}", input.name);
                    }
                    last_error = Some(error.clone());
                } else {
                    last_error = None;
                }
                let mut entry = Entry {
                    input,
                    captured_at_ns,
                    started,
                    completed,
                    interval: schedule.interval,
                    max_age: schedule.max_age,
                    actual_interval: previous_start
                        .map(|previous| started.duration_since(previous)),
                    sequence,
                    missed_periods,
                    result,
                };
                // Keep every completed observation. Backpressure pauses this producer
                // and is bounded; it never replaces a queued observation with a newer one.
                let until = Instant::now() + ingress_timeout;
                loop {
                    match sender.try_send(entry) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Disconnected(_)) => return Ok(()),
                        Err(mpsc::TrySendError::Full(value)) => entry = value,
                    }
                    ensure!(
                        Instant::now() < until,
                        "observation queue timeout; collector disabled, undelivered sequence {sequence}"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                ensure!(!expired, "collection deadline exceeded; collector disabled");
                previous_start = Some(started);
                // Queue waiting also consumes schedule slots. Account for those
                // once, without immediately running a catch-up collection.
                let now = Instant::now();
                deadline = if now >= next {
                    let (following, skipped) =
                        next_deadline(next - schedule.interval, schedule.interval, now);
                    missed_periods += skipped;
                    following
                } else {
                    next
                };
                control
                    .cancel
                    .wait(deadline.saturating_duration_since(Instant::now()));
            }
            Ok(())
        })?;
        self.collectors.push(Worker {
            input,
            start,
            task,
            failure: None,
        });
        Ok(())
    }
    pub fn add_assembler(&mut self, assembler: impl Assembler + 'static) -> Result<()> {
        ensure!(
            !self
                .assemblers
                .iter()
                .any(|existing| existing.name() == assembler.name()),
            "duplicate assembler {}",
            assembler.name()
        );
        for dependency in assembler.dependencies() {
            let entry = self.entries.get(dependency.name).ok_or_else(|| {
                anyhow!(
                    "assembler {} requires unregistered input {}",
                    assembler.name(),
                    dependency.name
                )
            })?;
            ensure!(
                entry.input.type_id == dependency.type_id,
                "assembler {}: input type mismatch for {} ({} vs {})",
                assembler.name(),
                dependency.name,
                entry.input.type_name,
                dependency.type_name
            );
        }
        self.assemblers.push(Box::new(assembler));
        Ok(())
    }
    fn accept(&mut self, entry: Entry) -> Result<()> {
        let trigger = entry.input.name;
        let captured_at_ns = entry.captured_at_ns;
        let observed_at = entry.completed;
        self.entries.insert(trigger, Arc::new(entry));
        for assembler in &mut self.assemblers {
            if !assembler
                .dependencies()
                .iter()
                .any(|input| input.name == trigger)
            {
                continue;
            }
            let entries = assembler
                .dependencies()
                .iter()
                .map(|input| (input.name, self.entries[input.name].clone()))
                .collect();
            let batch = Batch {
                id: self.batch_id,
                captured_at_ns,
                trigger,
                observed_at,
                entries,
            };
            for sample in assembler.assemble(&batch)? {
                self.dispatcher.publish(sample)?;
            }
        }
        // Late queued observations must still be recorded, but cannot revive a
        // source which supervision has disabled. Keep its retained state failed.
        if let Some(worker) = self
            .collectors
            .iter()
            .find(|worker| worker.input.name == trigger)
            && let Some(error) = &worker.failure
        {
            self.entries.insert(
                trigger,
                Arc::new(Entry::unavailable(worker.input, error.clone())),
            );
        }
        self.batch_id += 1;
        Ok(())
    }
    fn poll(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        for worker in &mut self.collectors {
            if worker.failure.is_none() {
                if let Some(error) = worker.task.poll().map(str::to_string) {
                    eprintln!("{error}");
                    worker.failure = Some(error.clone());
                    failures.push((worker.input, error));
                }
            } else {
                worker.task.poll();
            }
        }
        for (input, error) in failures {
            let mut entry = Entry::unavailable(input, error);
            entry.captured_at_ns = timestamp_ns();
            self.accept(entry)?;
        }
        ensure!(
            self.collectors.is_empty()
                || self
                    .collectors
                    .iter()
                    .any(|worker| worker.failure.is_none()),
            "all collectors have failed"
        );
        self.dispatcher.poll()
    }
    pub fn run(&mut self, duration: Option<Duration>, stop: Arc<AtomicBool>) -> Result<()> {
        ensure!(!self.started, "runtime can only be run once");
        self.started = true;
        let end = duration.map(|duration| Instant::now() + duration);
        for worker in &self.collectors {
            worker.start.send(())?;
        }
        let result: Result<()> = (|| {
            while !stop.load(Ordering::Relaxed) && end.is_none_or(|end| Instant::now() < end) {
                self.poll()?;
                match self.receiver.recv_timeout(Duration::from_millis(5)) {
                    Ok(entry) => self.accept(entry)?,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            Ok(())
        })();
        // One shared shutdown budget. Cancel producers, keep consuming pending
        // observations, then drain all output queues concurrently.
        let shutdown_started = Instant::now();
        let deadline = shutdown_started + self.options.shutdown_timeout;
        // Reserve at least half of the shutdown budget for draining/closing outputs.
        let collector_deadline = shutdown_started + self.options.shutdown_timeout / 2;
        for worker in &self.collectors {
            worker.task.control.cancel.cancel();
        }
        let mut errors = Vec::new();
        if let Err(error) = result {
            errors.push(format!("{error:#}"));
        }
        loop {
            if Instant::now() >= collector_deadline {
                errors.push(
                    "collector shutdown budget exhausted; queued observations may be incomplete"
                        .into(),
                );
                break;
            }
            for worker in &mut self.collectors {
                worker.task.poll();
            }
            match self.receiver.try_recv() {
                Ok(entry) => {
                    if let Err(error) = self.accept(entry) {
                        errors.push(format!("{error:#}"));
                    }
                }
                Err(_)
                    if self.collectors.iter().all(|worker| worker.task.finished())
                        || Instant::now() >= collector_deadline =>
                {
                    break;
                }
                Err(_) => thread::sleep(Duration::from_millis(1)),
            }
        }
        // Disconnect producers before joining so even pending queue sends stop.
        let (_, empty) = mpsc::sync_channel(1);
        let pending = std::mem::replace(&mut self.receiver, empty);
        let unassembled = pending.try_iter().count();
        drop(pending);
        if unassembled > 0 {
            errors.push(format!(
                "{unassembled} queued observations could not be assembled before shutdown"
            ));
        }
        for worker in &mut self.collectors {
            if let Err(error) = worker.task.finish(collector_deadline) {
                errors.push(format!("{error:#}"));
            }
        }
        if let Err(error) = self.dispatcher.finish(deadline) {
            errors.push(format!("{error:#}"));
        }
        errors.sort();
        errors.dedup();
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        for worker in &self.collectors {
            worker.task.control.cancel.cancel();
        }
    }
}
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
    #[test]
    fn scheduling_skips_overruns_without_catchup_bursts() {
        let start = Instant::now();
        let interval = Duration::from_millis(100);
        assert_eq!(
            next_deadline(start, interval, start + Duration::from_millis(250)),
            (start + Duration::from_millis(300), 2)
        );
        assert!(Schedule::new(Duration::ZERO, 3, interval).is_err());
        assert!(Schedule::new(interval, 0, interval).is_err());
        assert!(Schedule::new(interval, 3, Duration::ZERO).is_err());
    }
    #[test]
    fn late_observation_is_processed_without_reviving_a_disabled_source() {
        struct Empty;
        impl Collector for Empty {
            type Data = u64;
            fn collect(&mut self, _: &OperationContext) -> Result<u64> {
                Ok(0)
            }
        }
        let mut runtime = Runtime::new(
            Dispatcher::default(),
            RuntimeOptions {
                ingress_capacity: 4,
                ingress_timeout: Duration::from_secs(1),
                shutdown_timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        let key = Key::new("source");
        runtime
            .add_collector(
                key,
                Empty,
                Schedule::new(Duration::from_secs(1), 3, Duration::from_secs(1)).unwrap(),
            )
            .unwrap();
        runtime.collectors[0].failure = Some("timed out".into());
        let mut entry = Entry::unavailable(key.dependency(), String::new());
        entry.result = Ok(Box::new(42_u64));
        entry.sequence = 1;
        runtime.accept(entry).unwrap();
        assert_eq!(
            runtime.entries[key.name].result.as_ref().err().unwrap(),
            "timed out"
        );
    }
}

#[cfg(test)]
mod overload_tests {
    use super::*;
    #[test]
    fn full_ingress_retains_order_and_reports_undelivered_sequence() {
        struct Count(u64);
        impl Collector for Count {
            type Data = u64;
            fn collect(&mut self, _: &OperationContext) -> Result<u64> {
                self.0 += 1;
                Ok(self.0)
            }
        }
        let mut runtime = Runtime::new(
            Dispatcher::default(),
            RuntimeOptions {
                ingress_capacity: 2,
                ingress_timeout: Duration::from_millis(30),
                shutdown_timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        runtime
            .add_collector(
                Key::new("counter"),
                Count(0),
                Schedule::new(Duration::from_millis(1), 3, Duration::from_secs(1)).unwrap(),
            )
            .unwrap();
        runtime.collectors[0].start.send(()).unwrap();
        // Deliberately keep the consumer stopped until the bounded producer fails.
        let deadline = Instant::now() + Duration::from_secs(1);
        while !runtime.collectors[0].task.finished() && Instant::now() < deadline {
            runtime.collectors[0].task.poll();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            runtime.collectors[0]
                .task
                .poll()
                .unwrap()
                .contains("undelivered sequence 3")
        );
        let entries: Vec<_> = runtime.receiver.try_iter().collect();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }
}
