use crate::{
    data::GpuData,
    framework::{CollectContext, Collector, timestamp_ns},
};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LOAD_PATHS: &[&str] = &[
    "devices/gpu.0/load",
    "devices/platform/gpu.0/load",
    "devices/platform/17000000.gpu/load",
    "devices/platform/bus@0/17000000.gpu/load",
    "devices/platform/57000000.gpu/load",
];

pub struct JetsonCollector {
    load: Option<PathBuf>,
    executable: PathBuf,
    tegrastats: Option<Tegrastats>,
    timeout: Duration,
    interval: Duration,
}
impl JetsonCollector {
    /// Verify that a valid GPU observation is available before registering this collector.
    /// A successfully started tegrastats process is retained for periodic collection.
    pub fn probe(
        sys_root: &Path,
        load: Option<PathBuf>,
        executable: PathBuf,
        timeout: Duration,
        interval: Duration,
    ) -> Result<Self> {
        let load = load.or_else(|| {
            LOAD_PATHS
                .iter()
                .map(|path| sys_root.join(path))
                .find(|path| path.is_file())
        });
        let mut collector = Self {
            load,
            executable,
            tegrastats: None,
            timeout,
            interval,
        };
        collector
            .collect(&CollectContext::now())
            .context("cannot read a valid Jetson GPU sample")?;
        // The probe is not published; the first sampling batch reads fresh data.
        Ok(collector)
    }
}
impl Collector for JetsonCollector {
    type Data = GpuData;
    fn collect(&mut self, _: &CollectContext) -> Result<GpuData> {
        if let Some(path) = &self.load {
            let raw: f64 = fs::read_to_string(path)
                .with_context(|| path.display().to_string())?
                .trim()
                .parse()?;
            ensure!(
                raw.is_finite() && (0.0..=1000.0).contains(&raw),
                "invalid Jetson load {raw}"
            );
            return Ok(GpuData {
                utilization_percent: raw / 10.0,
                source: path.display().to_string(),
                observed_at_ns: timestamp_ns(),
            });
        }
        if self.tegrastats.is_none() {
            self.tegrastats = Some(Tegrastats::start(&self.executable, self.interval)?);
        }
        let result = self.tegrastats.as_mut().unwrap().next(self.timeout);
        if result.is_err() {
            self.tegrastats = None;
        }
        result
    }
}

fn parse_gr3d(line: &str) -> Result<f64> {
    let fields: Vec<_> = line.split_whitespace().collect();
    let index = fields
        .iter()
        .position(|field| *field == "GR3D_FREQ")
        .context("tegrastats has no GR3D_FREQ")?;
    let value = fields.get(index + 1).context("GR3D_FREQ has no value")?;
    let (percent, _) = value
        .split_once('%')
        .context("GR3D_FREQ reports frequency only, no utilization")?;
    let percent: f64 = percent.parse()?;
    ensure!(
        percent.is_finite() && (0.0..=100.0).contains(&percent),
        "invalid GR3D utilization"
    );
    Ok(percent)
}

struct Tegrastats {
    child: Child,
    receiver: Receiver<(Instant, u64, String)>,
    reader: Option<JoinHandle<()>>,
    interval: Duration,
}
impl Tegrastats {
    fn start(executable: &Path, interval: Duration) -> Result<Self> {
        let mut child = Command::new(executable)
            .arg("--interval")
            .arg(interval.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!(
                    "Jetson GPU load sysfs unavailable; cannot start {}",
                    executable.display()
                )
            })?;
        let stdout = child
            .stdout
            .take()
            .context("tegrastats stdout unavailable")?;
        let (sender, receiver) = mpsc::sync_channel(8);
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                match sender.try_send((Instant::now(), timestamp_ns(), line)) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        });
        Ok(Self {
            child,
            receiver,
            reader: Some(reader),
            interval,
        })
    }
    fn next(&mut self, timeout: Duration) -> Result<GpuData> {
        // Reuse only an unconsumed, recent observation from the persistent stream.
        // Waiting for a post-call line every time would add up to a full period
        // to each collection and halve the effective sampling rate.
        let deadline = Instant::now() + timeout;
        let mut latest = self.receiver.try_iter().last();
        let (observed_at_ns, line) = loop {
            let (received, timestamp, line) = match latest.take() {
                Some(sample) => sample,
                None => self
                    .receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|error| anyhow!("no fresh tegrastats sample: {error}"))?,
            };
            if received.elapsed() <= self.interval.saturating_mul(2) {
                break (timestamp, line);
            }
        };
        Ok(GpuData {
            utilization_percent: parse_gr3d(&line)?,
            source: "tegrastats/GR3D_FREQ".into(),
            observed_at_ns,
        })
    }
}
impl Drop for Tegrastats {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_xavier_orin_and_thor_and_rejects_missing_measurements() {
        for text in [
            "GR3D_FREQ 42%@76",
            "GR3D_FREQ 42%@[1098,1098]",
            "GR3D_FREQ 42%@[1098,1098,1098]",
            "GR3D_FREQ 42%",
        ] {
            assert_eq!(parse_gr3d(text).unwrap(), 42.0);
        }
        for text in [
            "GR3D_FREQ @[1098,1098]",
            "RAM 1/100MB",
            "GR3D_FREQ NaN%",
            "GR3D_FREQ 200%",
        ] {
            assert!(parse_gr3d(text).is_err());
        }
    }
    #[test]
    fn reads_jetson_sysfs_per_mille_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("load");
        fs::write(&path, "425\n").unwrap();
        let mut collector = JetsonCollector::probe(
            dir.path(),
            Some(path.clone()),
            "tegrastats".into(),
            Duration::from_secs(1),
            Duration::from_millis(100),
        )
        .unwrap();
        assert_eq!(
            collector
                .collect(&CollectContext::now())
                .unwrap()
                .utilization_percent,
            42.5
        );
        fs::write(path, "1001").unwrap();
        assert!(collector.collect(&CollectContext::now()).is_err());
    }

    #[test]
    fn startup_probe_accepts_idle_gpu_and_does_not_reuse_probe_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOAD_PATHS[0]);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "0\n").unwrap();
        let mut collector = JetsonCollector::probe(
            dir.path(),
            None,
            dir.path().join("no-tegrastats"),
            Duration::from_secs(1),
            Duration::from_millis(100),
        )
        .unwrap();
        fs::write(&path, "500\n").unwrap();
        assert_eq!(
            collector
                .collect(&CollectContext::now())
                .unwrap()
                .utilization_percent,
            50.0
        );
    }

    #[test]
    fn startup_probe_rejects_missing_or_invalid_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("load");
        let executable = dir.path().join("no-tegrastats");
        let timeout = Duration::from_secs(1);
        // Neither a sysfs node nor tegrastats is available.
        assert!(
            JetsonCollector::probe(
                dir.path(),
                None,
                executable.clone(),
                timeout,
                Duration::from_millis(100)
            )
            .is_err()
        );
        // An explicitly configured but unreadable node is not considered available.
        assert!(
            JetsonCollector::probe(
                dir.path(),
                Some(path.clone()),
                executable.clone(),
                timeout,
                Duration::from_millis(100)
            )
            .is_err()
        );
        for contents in ["not-a-number", "NaN", "1001"] {
            fs::write(&path, contents).unwrap();
            assert!(
                JetsonCollector::probe(
                    dir.path(),
                    Some(path.clone()),
                    executable.clone(),
                    timeout,
                    Duration::from_millis(100)
                )
                .is_err()
            );
        }
    }

    #[cfg(unix)]
    fn executable_script(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn startup_probe_reuses_tegrastats_for_periodic_collection() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("tegrastats");
        executable_script(
            &executable,
            "#!/bin/sh\nprintf 'started\\n' >> \"$0.starts\"\nprintf '%s\\n' \"$*\" > \"$0.args\"\nwhile :; do\n  printf '%s\\n' 'GR3D_FREQ 42%@[1098,1098]'\n  sleep 0.02\ndone\n",
        );
        let mut collector = JetsonCollector::probe(
            dir.path(),
            None,
            executable,
            Duration::from_secs(2),
            Duration::from_millis(37),
        )
        .unwrap();
        for _ in 0..3 {
            assert_eq!(
                collector
                    .collect(&CollectContext::now())
                    .unwrap()
                    .utilization_percent,
                42.0
            );
        }
        drop(collector);
        assert_eq!(
            fs::read_to_string(dir.path().join("tegrastats.args")).unwrap(),
            "--interval 37\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("tegrastats.starts")).unwrap(),
            "started\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_probe_times_out_when_tegrastats_produces_no_data() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("tegrastats");
        // exec keeps the sleeping process owned by the collector, allowing cleanup on timeout.
        executable_script(&executable, "#!/bin/sh\nexec sleep 30\n");
        let result = JetsonCollector::probe(
            dir.path(),
            None,
            executable,
            Duration::from_millis(50),
            Duration::from_millis(100),
        );
        let error = result
            .err()
            .expect("a silent process must fail the startup probe");
        assert!(format!("{error:#}").contains("no fresh tegrastats sample"));
    }
}
