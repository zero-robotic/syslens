use super::*;
use anyhow::{Result, bail};
use bytes::Bytes;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

fn descriptor() -> Arc<StreamDescriptor> {
    Arc::new(StreamDescriptor {
        topic: "/test".into(),
        message_encoding: "test".into(),
        schema_name: "test".into(),
        schema_encoding: "test".into(),
        schema: Bytes::new(),
    })
}
fn sample(n: u64) -> Sample {
    Sample {
        stream: descriptor(),
        captured_at_ns: n,
        sequence: 0,
        payload: Bytes::copy_from_slice(&n.to_le_bytes()),
    }
}
fn options() -> OutputOptions {
    OutputOptions {
        capacity: 1024,
        delivery: Delivery::All,
        overflow: Overflow::Disable,
        failure: FailurePolicy::Isolate,
        operation_timeout: Duration::from_secs(2),
    }
}
fn runtime_options() -> RuntimeOptions {
    RuntimeOptions {
        ingress_capacity: 128,
        ingress_timeout: Duration::from_secs(1),
        shutdown_timeout: Duration::from_secs(1),
    }
}
struct Capture(Arc<Mutex<Vec<Sample>>>);
impl Output for Capture {
    fn write(&mut self, sample: &Sample, _: &OperationContext) -> Result<()> {
        self.0.lock().unwrap().push(sample.clone());
        Ok(())
    }
    fn close(&mut self, _: &OperationContext) -> Result<()> {
        Ok(())
    }
}
struct Count(Arc<AtomicU64>);
impl Collector for Count {
    type Data = u64;
    fn collect(&mut self, _: &OperationContext) -> Result<u64> {
        Ok(self.0.fetch_add(1, Ordering::SeqCst) + 1)
    }
}
struct Assemble {
    key: Key<u64>,
    inputs: Vec<Dependency>,
}
impl Assemble {
    fn new(key: Key<u64>) -> Self {
        Self {
            key,
            inputs: vec![key.dependency()],
        }
    }
}
impl Assembler for Assemble {
    fn name(&self) -> &str {
        "test"
    }
    fn dependencies(&self) -> &[Dependency] {
        &self.inputs
    }
    fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>> {
        Ok(batch
            .read(self.key)?
            .map(|value| {
                let mut output = sample(*value);
                output.captured_at_ns = batch.captured_at_ns;
                output
            })
            .into_iter()
            .collect())
    }
}
#[test]
fn every_completed_observation_reaches_recording_while_live_is_throttled() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let live = Arc::new(Mutex::new(Vec::new()));
    let mut dispatcher = Dispatcher::default();
    dispatcher
        .add("record", Capture(recorded.clone()), options())
        .unwrap();
    dispatcher
        .add(
            "live",
            Capture(live.clone()),
            OutputOptions {
                delivery: Delivery::Latest {
                    interval: Duration::from_millis(80),
                },
                ..options()
            },
        )
        .unwrap();
    let mut runtime = Runtime::new(dispatcher, runtime_options()).unwrap();
    let count = Arc::new(AtomicU64::new(0));
    let key = Key::new("counter");
    runtime
        .add_collector(
            key,
            Count(count.clone()),
            Schedule::new(Duration::from_millis(2), 3, Duration::from_secs(1)).unwrap(),
        )
        .unwrap();
    runtime.add_assembler(Assemble::new(key)).unwrap();
    runtime
        .run(
            Some(Duration::from_millis(150)),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let recorded = recorded.lock().unwrap();
    let live = live.lock().unwrap();
    assert!(recorded.len() >= 10);
    assert_eq!(recorded.len() as u64, count.load(Ordering::SeqCst));
    assert!(live.len() < recorded.len());
    for (index, value) in recorded.iter().enumerate() {
        assert_eq!(
            u64::from_le_bytes(value.payload[..].try_into().unwrap()),
            index as u64 + 1
        );
        assert_eq!(value.sequence, index as u32);
        assert!(value.captured_at_ns > 0);
    }
    for value in live.iter() {
        let original = &recorded[value.sequence as usize];
        assert_eq!(value.payload.as_ptr(), original.payload.as_ptr());
        assert_eq!(value.captured_at_ns, original.captured_at_ns);
    }
}
struct Gate {
    entered: Option<mpsc::Sender<()>>,
    release: mpsc::Receiver<()>,
    values: Arc<Mutex<Vec<Sample>>>,
}
impl Output for Gate {
    fn write(&mut self, sample: &Sample, context: &OperationContext) -> Result<()> {
        if let Some(entered) = self.entered.take() {
            entered.send(())?;
            while self.release.try_recv().is_err() {
                context.wait(Duration::from_millis(2))?;
            }
        }
        self.values.lock().unwrap().push(sample.clone());
        Ok(())
    }
    fn close(&mut self, _: &OperationContext) -> Result<()> {
        Ok(())
    }
}
#[test]
fn recording_overload_isolated_and_accepted_messages_drained() {
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let live = Arc::new(Mutex::new(Vec::new()));
    let mut dispatcher = Dispatcher::default();
    dispatcher
        .add(
            "record",
            Gate {
                entered: Some(entered),
                release: gate,
                values: recorded.clone(),
            },
            OutputOptions {
                capacity: 1,
                ..options()
            },
        )
        .unwrap();
    dispatcher
        .add("live", Capture(live.clone()), options())
        .unwrap();
    dispatcher.publish(sample(1)).unwrap();
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    dispatcher.publish(sample(2)).unwrap();
    for n in 3..=20 {
        dispatcher.publish(sample(n)).unwrap();
    }
    release.send(()).unwrap();
    assert!(
        dispatcher
            .finish(Instant::now() + Duration::from_secs(1))
            .is_err()
    );
    assert_eq!(recorded.lock().unwrap().len(), 2);
    assert_eq!(live.lock().unwrap().len(), 20);
}
struct Fail;
impl Output for Fail {
    fn write(&mut self, _: &Sample, _: &OperationContext) -> Result<()> {
        bail!("simulated failure")
    }
    fn close(&mut self, _: &OperationContext) -> Result<()> {
        Ok(())
    }
}
#[test]
fn failed_live_output_does_not_stop_recording() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut dispatcher = Dispatcher::default();
    dispatcher.add("live", Fail, options()).unwrap();
    dispatcher
        .add("record", Capture(recorded.clone()), options())
        .unwrap();
    for n in 0..20 {
        dispatcher.publish(sample(n)).unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        dispatcher
            .finish(Instant::now() + Duration::from_secs(1))
            .is_err()
    );
    assert_eq!(recorded.lock().unwrap().len(), 20);
}
#[test]
fn registry_rejects_wrong_types_and_undeclared_reads_are_errors() {
    let mut runtime = Runtime::new(Dispatcher::default(), runtime_options()).unwrap();
    runtime
        .add_unavailable(Key::<String>::new("same_name"), "disabled".into())
        .unwrap();
    assert!(
        runtime
            .add_assembler(Assemble::new(Key::new("same_name")))
            .unwrap_err()
            .to_string()
            .contains("type mismatch")
    );
    let mut batch = Batch::for_test(0);
    batch.insert(Key::<u64>::new("x"), 7);
    assert!(batch.read(Key::<String>::new("x")).is_err());
    assert!(batch.read(Key::<u64>::new("missing")).is_err());
    batch.unavailable(Key::<u64>::new("optional"), "not supported");
    assert!(batch.read(Key::<u64>::new("optional")).unwrap().is_none());
}
struct Stuck(mpsc::Sender<()>, mpsc::Receiver<()>);
impl Collector for Stuck {
    type Data = u64;
    fn collect(&mut self, _: &OperationContext) -> Result<u64> {
        self.0.send(())?;
        self.1.recv()?;
        Ok(1)
    }
}
#[test]
fn uncooperative_collector_cannot_hold_shutdown_indefinitely() {
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let mut runtime = Runtime::new(
        Dispatcher::default(),
        RuntimeOptions {
            shutdown_timeout: Duration::from_millis(80),
            ..runtime_options()
        },
    )
    .unwrap();
    runtime
        .add_collector(
            Key::new("stuck"),
            Stuck(entered, gate),
            Schedule::new(Duration::from_secs(1), 3, Duration::from_secs(5)).unwrap(),
        )
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    let worker = thread::spawn(move || runtime.run(None, signal));
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    let start = Instant::now();
    stop.store(true, Ordering::SeqCst);
    let result = worker.join().unwrap();
    release.send(()).unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("shutdown deadline")
    );
    assert!(start.elapsed() < Duration::from_millis(300));
}
struct Cooperative;
impl Collector for Cooperative {
    type Data = u64;
    fn collect(&mut self, context: &OperationContext) -> Result<u64> {
        context.wait(Duration::from_secs(10))?;
        Ok(1)
    }
}
#[test]
fn slow_source_does_not_block_fast_observations_and_cancels_promptly() {
    let values = Arc::new(Mutex::new(Vec::new()));
    let mut dispatcher = Dispatcher::default();
    dispatcher
        .add("record", Capture(values.clone()), options())
        .unwrap();
    let mut runtime = Runtime::new(dispatcher, runtime_options()).unwrap();
    runtime
        .add_collector(
            Key::new("slow"),
            Cooperative,
            Schedule::new(Duration::from_secs(1), 3, Duration::from_secs(5)).unwrap(),
        )
        .unwrap();
    let key = Key::new("fast");
    runtime
        .add_collector(
            key,
            Count(Arc::new(AtomicU64::new(0))),
            Schedule::new(Duration::from_millis(5), 3, Duration::from_secs(1)).unwrap(),
        )
        .unwrap();
    runtime.add_assembler(Assemble::new(key)).unwrap();
    let start = Instant::now();
    runtime
        .run(
            Some(Duration::from_millis(80)),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert!(values.lock().unwrap().len() > 5);
    assert!(start.elapsed() < Duration::from_millis(400));
}

#[test]
fn output_deadline_isolated_and_shutdown_is_bounded_even_for_uncooperative_io() {
    struct StuckOutput {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }
    impl Output for StuckOutput {
        fn write(&mut self, _: &Sample, _: &OperationContext) -> Result<()> {
            if let Some(entered) = self.entered.take() {
                entered.send(())?;
                self.release.recv()?;
            }
            Ok(())
        }
        fn close(&mut self, _: &OperationContext) -> Result<()> {
            Ok(())
        }
    }
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let live = Arc::new(Mutex::new(Vec::new()));
    let mut dispatcher = Dispatcher::default();
    dispatcher
        .add(
            "stuck",
            StuckOutput {
                entered: Some(entered),
                release: gate,
            },
            OutputOptions {
                operation_timeout: Duration::from_millis(20),
                ..options()
            },
        )
        .unwrap();
    dispatcher
        .add("healthy", Capture(live.clone()), options())
        .unwrap();
    dispatcher.publish(sample(1)).unwrap();
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    thread::sleep(Duration::from_millis(30));
    dispatcher.poll().unwrap();
    dispatcher.publish(sample(2)).unwrap();
    let started = Instant::now();
    assert!(
        dispatcher
            .finish(Instant::now() + Duration::from_millis(50))
            .is_err()
    );
    release.send(()).unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(live.lock().unwrap().len(), 2);
}
#[test]
fn shutdown_reserves_time_to_finalize_healthy_outputs() {
    struct CloseNotice(mpsc::Sender<()>);
    impl Output for CloseNotice {
        fn write(&mut self, _: &Sample, _: &OperationContext) -> Result<()> {
            Ok(())
        }
        fn close(&mut self, _: &OperationContext) -> Result<()> {
            self.0.send(())?;
            Ok(())
        }
    }
    let (closed, close_rx) = mpsc::channel();
    let mut dispatcher = Dispatcher::default();
    dispatcher
        .add("record", CloseNotice(closed), options())
        .unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let mut runtime = Runtime::new(
        dispatcher,
        RuntimeOptions {
            shutdown_timeout: Duration::from_millis(150),
            ..runtime_options()
        },
    )
    .unwrap();
    runtime
        .add_collector(
            Key::new("stuck"),
            Stuck(entered, gate),
            Schedule::new(Duration::from_secs(1), 3, Duration::from_secs(10)).unwrap(),
        )
        .unwrap();
    let signal = Arc::new(AtomicBool::new(false));
    let stop = signal.clone();
    let worker = thread::spawn(move || runtime.run(None, signal));
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    stop.store(true, Ordering::SeqCst);
    assert!(worker.join().unwrap().is_err());
    release.send(()).unwrap();
    close_rx.recv_timeout(Duration::from_millis(20)).unwrap();
}
#[test]
fn fail_fast_output_policy_is_explicit() {
    let mut dispatcher = Dispatcher::default();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    dispatcher
        .add(
            "required",
            Gate {
                entered: Some(entered),
                release: gate,
                values: Arc::new(Mutex::new(Vec::new())),
            },
            OutputOptions {
                capacity: 1,
                failure: FailurePolicy::Stop,
                ..options()
            },
        )
        .unwrap();
    dispatcher
        .add(
            "other",
            Capture(Arc::new(Mutex::new(Vec::new()))),
            options(),
        )
        .unwrap();
    dispatcher.publish(sample(1)).unwrap();
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    dispatcher.publish(sample(2)).unwrap();
    assert!(dispatcher.publish(sample(3)).is_err());
    release.send(()).unwrap();
    assert!(
        dispatcher
            .finish(Instant::now() + Duration::from_secs(1))
            .is_err()
    );
}

#[test]
fn startup_work_uses_bounded_supervision_too() {
    let (release, gate) = mpsc::channel();
    let started = Instant::now();
    let result = run_bounded(
        "startup",
        Duration::from_millis(30),
        Duration::from_millis(30),
        Cancellation::default(),
        move |_| {
            gate.recv()?;
            Ok(42)
        },
    );
    release.send(()).unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("deadline exceeded")
    );
    assert!(started.elapsed() < Duration::from_millis(200));
}
