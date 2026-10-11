use super::command::Pipe;
use crate::{
    data::GpuData,
    framework::{Collector, OperationContext, timestamp_ns},
};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
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
    interval: Duration,
}
impl JetsonCollector {
    pub fn probe(
        sys_root: &Path,
        explicit: Option<PathBuf>,
        executable: PathBuf,
        timeout: Duration,
        interval: Duration,
        context: &OperationContext,
    ) -> Result<Self> {
        let paths: Vec<_> = explicit.map_or_else(
            || LOAD_PATHS.iter().map(|path| sys_root.join(path)).collect(),
            |path| vec![path],
        );
        let mut errors = Vec::new();
        for path in paths {
            context.check()?;
            let mut source = Self {
                load: Some(path),
                executable: executable.clone(),
                tegrastats: None,
                timeout,
                interval,
            };
            match source.collect(context) {
                Ok(_) => return Ok(source),
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        // Existing but unreadable/invalid sysfs nodes do not prevent fallback.
        let mut source = Self {
            load: None,
            executable,
            tegrastats: None,
            timeout,
            interval,
        };
        source.collect(context).with_context(|| {
            format!(
                "sysfs sources unavailable ({}); tegrastats probe",
                errors.join("; ")
            )
        })?;
        Ok(source)
    }
}
impl Collector for JetsonCollector {
    type Data = GpuData;
    fn collect(&mut self, context: &OperationContext) -> Result<GpuData> {
        context.check()?;
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
        let result = self
            .tegrastats
            .as_mut()
            .unwrap()
            .next(&context.limited(self.timeout));
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
    pipe: Pipe,
    buffer: Vec<u8>,
}
impl Tegrastats {
    fn start(executable: &Path, interval: Duration) -> Result<Self> {
        Ok(Self {
            pipe: Pipe::spawn(
                executable,
                &["--interval", &interval.as_millis().to_string()],
            )?,
            buffer: Vec::new(),
        })
    }
    fn next(&mut self, context: &OperationContext) -> Result<GpuData> {
        loop {
            context.check()?;
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=end).collect();
                return Ok(GpuData {
                    utilization_percent: parse_gr3d(std::str::from_utf8(&line)?)?,
                    source: "tegrastats/GR3D_FREQ".into(),
                    observed_at_ns: timestamp_ns(),
                });
            }
            ensure!(!self.pipe.read(&mut self.buffer)?, "tegrastats exited");
            context.wait(Duration::from_millis(2))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::Instant};
    fn script(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[test]
    fn parses_orin_and_thor_without_inventing_missing_measurements() {
        assert_eq!(parse_gr3d("GR3D_FREQ 40%@[1098,1098]").unwrap(), 40.0);
        assert_eq!(parse_gr3d("GR3D_FREQ 0%").unwrap(), 0.0);
        assert!(parse_gr3d("GR3D_FREQ @[1098,1098]").is_err());
        assert!(parse_gr3d("GR3D_FREQ 101%").is_err());
    }
    #[test]
    fn sysfs_probe_accepts_idle_and_collects_fresh_data() {
        let dir = tempfile::tempdir().unwrap();
        let load = dir.path().join("load");
        fs::write(&load, "0").unwrap();
        let mut source = JetsonCollector::probe(
            dir.path(),
            Some(load.clone()),
            dir.path().join("missing"),
            Duration::from_millis(100),
            Duration::from_millis(10),
            &OperationContext::now(),
        )
        .unwrap();
        fs::write(&load, "375").unwrap();
        assert_eq!(
            source
                .collect(&OperationContext::now())
                .unwrap()
                .utilization_percent,
            37.5
        );
    }
    #[test]
    fn invalid_sysfs_falls_back_to_one_persistent_child() {
        let dir = tempfile::tempdir().unwrap();
        let load = dir.path().join("load");
        fs::write(&load, "invalid").unwrap();
        let executable = dir.path().join("tegra");
        script(
            &executable,
            "#!/bin/sh\necho started >> \"$0.starts\"\nwhile :; do echo 'GR3D_FREQ 12%'; sleep 0.01; done\n",
        );
        let mut source = JetsonCollector::probe(
            dir.path(),
            Some(load),
            executable.clone(),
            Duration::from_millis(200),
            Duration::from_millis(10),
            &OperationContext::now(),
        )
        .unwrap();
        for _ in 0..3 {
            assert_eq!(
                source
                    .collect(&OperationContext::now())
                    .unwrap()
                    .utilization_percent,
                12.0
            );
        }
        drop(source);
        assert_eq!(
            fs::read_to_string(executable.with_extension("starts")).unwrap(),
            "started\n"
        );
    }
    #[test]
    fn silent_child_and_inherited_stdout_obey_deadline_and_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("tegra");
        script(&executable, "#!/bin/sh\nsleep 10 &\nwait\n");
        let started = Instant::now();
        assert!(
            JetsonCollector::probe(
                dir.path(),
                None,
                executable,
                Duration::from_millis(50),
                Duration::from_millis(10),
                &OperationContext::now()
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
