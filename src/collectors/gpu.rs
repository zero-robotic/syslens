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
    time::Duration,
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
}
impl JetsonCollector {
    pub fn new(
        sys_root: &Path,
        load: Option<PathBuf>,
        executable: PathBuf,
        timeout: Duration,
    ) -> Self {
        let load = load.or_else(|| {
            LOAD_PATHS
                .iter()
                .map(|path| sys_root.join(path))
                .find(|path| path.is_file())
        });
        Self {
            load,
            executable,
            tegrastats: None,
            timeout,
        }
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
            self.tegrastats = Some(Tegrastats::start(&self.executable)?);
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
    receiver: Receiver<(u64, String)>,
    reader: Option<JoinHandle<()>>,
}
impl Tegrastats {
    fn start(executable: &Path) -> Result<Self> {
        let mut child = Command::new(executable)
            .args(["--interval", "100"])
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
                match sender.try_send((timestamp_ns(), line)) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        });
        Ok(Self {
            child,
            receiver,
            reader: Some(reader),
        })
    }
    fn next(&mut self, timeout: Duration) -> Result<GpuData> {
        // Discard buffered lines; wait for a new observation for this batch.
        while self.receiver.try_recv().is_ok() {}
        let (observed_at_ns, line) = self
            .receiver
            .recv_timeout(timeout)
            .map_err(|error| anyhow!("no fresh tegrastats sample: {error}"))?;
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
        let mut collector = JetsonCollector::new(
            dir.path(),
            Some(path.clone()),
            "tegrastats".into(),
            Duration::from_secs(1),
        );
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
}
