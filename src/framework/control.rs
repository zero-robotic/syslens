//! Cooperative cancellation and bounded supervision shared by all worker types.
use super::timestamp_ns;
use anyhow::{Result, anyhow, ensure};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn wait(&self, duration: Duration) {
        let until = Instant::now() + duration;
        while !self.is_cancelled() && Instant::now() < until {
            thread::sleep(
                until
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(5)),
            );
        }
    }
}

#[derive(Clone)]
pub struct OperationContext {
    pub captured_at_ns: u64,
    cancellation: Cancellation,
    deadline: Instant,
}
impl OperationContext {
    pub fn new(cancellation: Cancellation, timeout: Duration) -> Self {
        Self {
            captured_at_ns: timestamp_ns(),
            cancellation,
            deadline: Instant::now() + timeout,
        }
    }
    pub fn now() -> Self {
        Self::new(Cancellation::default(), Duration::from_secs(30))
    }
    pub fn check(&self) -> Result<()> {
        ensure!(!self.cancellation.is_cancelled(), "operation cancelled");
        self.check_deadline()
    }
    pub fn check_deadline(&self) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "operation deadline exceeded"
        );
        Ok(())
    }
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    pub fn limited(&self, timeout: Duration) -> Self {
        Self {
            deadline: self.deadline.min(Instant::now() + timeout),
            ..self.clone()
        }
    }
    pub fn wait(&self, duration: Duration) -> Result<()> {
        self.check()?;
        self.cancellation.wait(duration.min(self.remaining()));
        self.check()
    }
}

#[derive(Clone, Default)]
pub(super) struct Control {
    pub cancel: Cancellation,
    active: Arc<Mutex<Option<Instant>>>,
}
impl Control {
    pub fn operation(&self, timeout: Duration) -> Operation {
        let context = OperationContext::new(self.cancel.clone(), timeout);
        *self.active.lock().unwrap() = Some(context.deadline);
        Operation {
            context,
            active: self.active.clone(),
        }
    }
}
pub(super) struct Operation {
    pub context: OperationContext,
    active: Arc<Mutex<Option<Instant>>>,
}
impl Drop for Operation {
    fn drop(&mut self) {
        *self.active.lock().unwrap() = None;
    }
}

pub(super) struct Task {
    name: String,
    pub control: Control,
    handle: Option<JoinHandle<Result<()>>>,
    failure: Option<String>,
}
impl Task {
    pub fn spawn(
        name: String,
        run: impl FnOnce(Control) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let control = Control::default();
        let worker_control = control.clone();
        let handle = thread::Builder::new()
            .name(name.clone())
            .spawn(move || run(worker_control))?;
        Ok(Self {
            name,
            control,
            handle: Some(handle),
            failure: None,
        })
    }
    pub fn poll(&mut self) -> Option<&str> {
        if self.failure.is_none()
            && self
                .control
                .active
                .lock()
                .unwrap()
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.failure = Some(format!("{}: operation deadline exceeded", self.name));
            self.control.cancel.cancel();
        }
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            let result = self
                .handle
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err(anyhow!("worker panicked")));
            if let Err(error) = result {
                self.failure
                    .get_or_insert_with(|| format!("{}: {error:#}", self.name));
            }
        }
        self.failure.as_deref()
    }
    pub fn finished(&self) -> bool {
        self.handle.is_none()
    }
    pub fn finish(&mut self, deadline: Instant) -> Result<()> {
        self.poll();
        while self.handle.is_some() && Instant::now() < deadline {
            self.poll();
            if self.handle.is_some() {
                thread::sleep(Duration::from_millis(2));
            }
        }
        if self.handle.is_some() {
            self.control.cancel.cancel();
            self.handle.take(); // Rust cannot forcibly terminate an uncooperative thread.
            self.failure.get_or_insert_with(|| {
                format!("{}: shutdown deadline exceeded; worker detached", self.name)
            });
        }
        match &self.failure {
            Some(error) => Err(anyhow!(error.clone())),
            None => Ok(()),
        }
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.control.cancel.cancel();
    }
}

/// Run startup work under the same supervision as long-lived workers. Its result
/// remains owned by the worker until delivery; a late result is dropped there.
pub fn run_bounded<T: Send + 'static>(
    name: &str,
    timeout: Duration,
    shutdown_timeout: Duration,
    cancellation: Cancellation,
    run: impl FnOnce(&OperationContext) -> Result<T> + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let deadline = Instant::now() + timeout;
    let mut task = Task::spawn(name.into(), move |control| {
        let operation = control.operation(timeout);
        let result = run(&operation.context).and_then(|value| {
            operation.context.check()?;
            Ok(value)
        });
        let _ = sender.send(result);
        Ok(())
    })?;
    loop {
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            task.control.cancel.cancel();
            let reason = if cancellation.is_cancelled() {
                "cancelled"
            } else {
                "deadline exceeded"
            };
            let _ = task.finish(deadline.min(Instant::now() + shutdown_timeout));
            return Err(anyhow!("{name}: {reason}"));
        }
        match receiver.recv_timeout(Duration::from_millis(2)) {
            Ok(result) => {
                task.finish(deadline)?;
                return result;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                task.finish(deadline)?;
                return Err(anyhow!("{name}: task ended without a result"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                task.poll();
            }
        }
    }
}
