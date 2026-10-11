use super::procfs::{Selector, read_stat};
use crate::{
    data::{GpuAccountingData, GpuData, GpuMetricsData},
    framework::{CollectContext, Collector, timestamp_ns},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MIB: u64 = 1024 * 1024;
const DEVICE_QUERY: &[&str] = &[
    "--query-gpu=name,utilization.gpu,memory.total,memory.used",
    "--format=csv,noheader,nounits",
    "-i",
    "0",
];
const PROCESS_QUERY: &[&str] = &[
    "--query-compute-apps=pid,used_memory",
    "--format=csv,noheader,nounits",
    "-i",
    "0",
];
const UTILIZATION_QUERY: &[&str] = &["pmon", "-i", "0", "-s", "u", "-c", "1"];

#[derive(Clone)]
enum Backend {
    Nvmap(PathBuf),
    Smi {
        executable: PathBuf,
        // Failed startup capabilities stay disabled for this run.
        device: Result<(), String>,
        load: Result<(), String>,
        memory: Result<(), String>,
        utilization: Result<(), String>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Metric {
    Memory,
    ProcessUtilization,
}

pub struct GpuCollectors {
    pub memory: Result<GpuAccountingCollector, String>,
    pub process_utilization: Result<GpuAccountingCollector, String>,
    pub system_utilization: Result<SmiLoadCollector, String>,
}

pub struct SmiLoadCollector {
    executable: PathBuf,
    timeout: Duration,
}
impl Collector for SmiLoadCollector {
    type Data = GpuData;
    fn collect(&mut self, _: &CollectContext) -> Result<Self::Data> {
        let sample = parse_device(&command(&self.executable, DEVICE_QUERY, self.timeout)?)?;
        Ok(GpuData {
            utilization_percent: sample
                .utilization_percent
                .context("nvidia-smi system utilization N/A")?,
            source: sample.utilization_source,
            observed_at_ns: sample.utilization_timestamp_ns.unwrap_or_else(timestamp_ns),
        })
    }
}

#[derive(Clone)]
pub struct GpuAccountingCollector {
    backend: Backend,
    metric: Metric,
    proc_root: PathBuf,
    selector: Option<Selector>,
    timeout: Duration,
}

impl GpuAccountingCollector {
    pub fn probe(
        nvmap_clients: PathBuf,
        executable: PathBuf,
        timeout: Duration,
        proc_root: PathBuf,
        selector: Option<Selector>,
    ) -> Result<Self> {
        let nvmap = fs::read_to_string(&nvmap_clients)
            .with_context(|| nvmap_clients.display().to_string())
            .and_then(|text| parse_nvmap(&text));
        let backend = match nvmap {
            Ok(_) => Backend::Nvmap(nvmap_clients),
            Err(nvmap_error) => {
                // A real GPU name rejects Orin's nvidia-smi display-driver stub.
                let device_sample = command(&executable, DEVICE_QUERY, timeout)
                    .and_then(|text| parse_device(&text))
                    .with_context(|| {
                        format!("nvmap unavailable ({nvmap_error:#}); nvidia-smi probe")
                    })?;
                let load = device_sample
                    .utilization_percent
                    .map(|_| ())
                    .ok_or_else(|| "nvidia-smi system utilization N/A".to_string());
                let device = if device_sample.memory_used_bytes.is_some()
                    || device_sample.memory_total_bytes.is_some()
                {
                    Ok(())
                } else {
                    Err("nvidia-smi system memory is N/A".into())
                };
                let (memory, utilization) = if selector.is_some() {
                    (
                        command(&executable, PROCESS_QUERY, timeout)
                            .and_then(|text| parse_process_memory(&text))
                            .and_then(probe_values)
                            .map_err(|error| format!("{error:#}")),
                        command(&executable, UTILIZATION_QUERY, timeout)
                            .and_then(|text| parse_process_utilization(&text))
                            .and_then(probe_values)
                            .map_err(|error| format!("{error:#}")),
                    )
                } else {
                    (
                        Err("process collection disabled".into()),
                        Err("process collection disabled".into()),
                    )
                };
                ensure!(
                    load.is_ok() || device.is_ok() || memory.is_ok() || utilization.is_ok(),
                    "no supported nvidia-smi metrics: system={device:?}, memory={memory:?}, utilization={utilization:?}"
                );
                Backend::Smi {
                    executable,
                    device,
                    load,
                    memory,
                    utilization,
                }
            }
        };
        Ok(Self {
            backend,
            metric: Metric::Memory,
            proc_root,
            selector,
            timeout,
        })
    }

    pub fn split(self) -> GpuCollectors {
        let (memory, utilization, system_utilization) = match &self.backend {
            Backend::Nvmap(_) => (
                Ok(()),
                Err("per-process GPU utilization is not exposed by Orin nvmap".to_string()),
                Err("nvmap does not expose system GPU utilization".to_string()),
            ),
            Backend::Smi {
                executable,
                device,
                load,
                memory,
                utilization,
            } => (
                if device.is_ok() || memory.is_ok() {
                    Ok(())
                } else {
                    Err(format!(
                        "system memory: {}; process memory: {}",
                        device.as_ref().unwrap_err(),
                        memory.as_ref().unwrap_err()
                    ))
                },
                utilization.clone(),
                load.clone().map(|_| SmiLoadCollector {
                    executable: executable.clone(),
                    timeout: self.timeout,
                }),
            ),
        };
        let process_utilization = utilization.map(|_| {
            let mut collector = self.clone();
            collector.metric = Metric::ProcessUtilization;
            collector
        });
        GpuCollectors {
            memory: memory.map(|_| self),
            process_utilization,
            system_utilization,
        }
    }

    fn identities(&self) -> BTreeMap<u32, u64> {
        let Some(selector) = &self.selector else {
            return BTreeMap::new();
        };
        selector
            .candidates(&self.proc_root)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|pid| {
                let stat = read_stat(&self.proc_root.join(pid.to_string()).join("stat")).ok()?;
                (stat.pid == pid && selector.includes(&stat)).then_some((pid, stat.start_ticks))
            })
            .collect()
    }
}

impl Collector for GpuAccountingCollector {
    type Data = GpuAccountingData;
    fn collect(&mut self, _: &CollectContext) -> Result<Self::Data> {
        // Read identities on both sides of the driver query. Never attach an exited
        // process's GPU allocation to a newly created process with the same PID.
        let before = self.identities();
        let (system, by_pid, missing_process) = match &self.backend {
            Backend::Nvmap(path) => {
                let (total, clients) = parse_nvmap(
                    &fs::read_to_string(path).with_context(|| path.display().to_string())?,
                )?;
                let now = timestamp_ns();
                let missing = GpuMetricsData {
                    utilization_error: Some(
                        "per-process GPU utilization is not exposed by Orin nvmap".into(),
                    ),
                    memory_error: Some("process is absent from the nvmap client snapshot".into()),
                    memory_source: path.display().to_string(),
                    memory_scope:
                        "nvmap_client_allocations; may include multimedia/shared allocations".into(),
                    ..Default::default()
                };
                let system = GpuMetricsData {
                    memory_used_bytes: Some(total),
                    memory_source: path.display().to_string(),
                    memory_scope:
                        "nvmap_total_allocations; not GPU-exclusive or a sum of process rows".into(),
                    memory_timestamp_ns: Some(now),
                    utilization_error: Some("nvmap does not expose system GPU utilization".into()),
                    ..Default::default()
                };
                let rows = clients
                    .into_iter()
                    .map(|(pid, bytes)| {
                        (
                            pid,
                            GpuMetricsData {
                                memory_used_bytes: Some(bytes),
                                memory_error: None,
                                memory_timestamp_ns: Some(now),
                                ..missing.clone()
                            },
                        )
                    })
                    .collect();
                (system, rows, missing)
            }
            Backend::Smi {
                executable,
                device,
                memory,
                utilization,
                ..
            } => {
                let system = if self.metric == Metric::Memory {
                    match available_query(device, executable, DEVICE_QUERY, self.timeout)
                        .and_then(|text| parse_device(&text))
                    {
                        Ok(mut data) => {
                            data.utilization_percent = None;
                            data.utilization_timestamp_ns = None;
                            data.utilization_source.clear();
                            data.utilization_error = None;
                            data
                        }
                        Err(error) => GpuMetricsData {
                            memory_error: Some(format!("{error:#}")),
                            ..Default::default()
                        },
                    }
                } else {
                    GpuMetricsData::default()
                };
                let memory_result = if self.metric == Metric::Memory {
                    available_query(memory, executable, PROCESS_QUERY, self.timeout)
                        .and_then(|text| parse_process_memory(&text))
                } else {
                    Ok(BTreeMap::new())
                };
                let memory_time = timestamp_ns();
                let utilization_result = if self.metric == Metric::ProcessUtilization {
                    available_query(utilization, executable, UTILIZATION_QUERY, self.timeout)
                        .and_then(|text| parse_process_utilization(&text))
                } else {
                    Ok(BTreeMap::new())
                };
                let utilization_time = timestamp_ns();
                let missing = GpuMetricsData {
                    memory_source: "nvidia-smi/compute-apps/used_memory".into(),
                    memory_scope:
                        "compute_process_gpu_allocation; graphics-only processes are not covered"
                            .into(),
                    utilization_source: "nvidia-smi/pmon/sm".into(),
                    memory_error: Some(match &memory_result {
                        Ok(_) => "process is absent from the compute-apps snapshot".into(),
                        Err(error) => format!("{error:#}"),
                    }),
                    utilization_error: Some(match &utilization_result {
                        Ok(_) => "process is absent from the pmon snapshot".into(),
                        Err(error) => format!("{error:#}"),
                    }),
                    ..Default::default()
                };
                let mut rows: BTreeMap<u32, GpuMetricsData> = BTreeMap::new();
                for (pid, bytes) in memory_result.unwrap_or_default() {
                    let data = rows.entry(pid).or_insert_with(|| missing.clone());
                    data.memory_used_bytes = bytes;
                    data.memory_timestamp_ns = bytes.map(|_| memory_time);
                    data.memory_error = bytes
                        .is_none()
                        .then(|| "nvidia-smi reports process memory N/A".into());
                }
                for (pid, percent) in utilization_result.unwrap_or_default() {
                    let data = rows.entry(pid).or_insert_with(|| missing.clone());
                    data.utilization_percent = percent;
                    data.utilization_timestamp_ns = percent.map(|_| utilization_time);
                    data.utilization_error = percent
                        .is_none()
                        .then(|| "nvidia-smi reports process utilization N/A".into());
                }
                (system, rows, missing)
            }
        };
        let after = self.identities();
        let processes = by_pid
            .into_iter()
            .filter_map(|(pid, data)| {
                let start = before.get(&pid)?;
                (after.get(&pid) == Some(start)).then_some(((pid, *start), data))
            })
            .collect();
        Ok(GpuAccountingData {
            system,
            processes,
            missing_process,
        })
    }
}

fn available_query(
    capability: &Result<(), String>,
    executable: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String> {
    capability
        .as_ref()
        .map_err(|error| anyhow!(error.clone()))?;
    command(executable, args, timeout)
}

fn probe_values<T>(rows: BTreeMap<u32, Option<T>>) -> Result<()> {
    // Empty tables cannot establish per-process support, but must remain enabled
    // for processes that start later. Explicit N/A for all active rows can.
    ensure!(
        rows.is_empty() || rows.values().any(Option::is_some),
        "nvidia-smi reports N/A for all active processes"
    );
    Ok(())
}

// No shell is used. The timeout bounds a wedged driver/tool and output is capped.
fn command(executable: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    const LIMIT: u64 = 4 * 1024 * 1024;
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start {}", executable.display()))?;
    let mut stdout = child
        .stdout
        .take()
        .context("nvidia-smi stdout unavailable")?;
    let result = (|| {
        // Nonblocking reads keep the timeout effective even if an executable's
        // descendant inherits stdout. There is no reader thread left to join.
        let fd = stdout.as_raw_fd();
        // fcntl acts on our owned pipe descriptor; no pointers are passed.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(flags >= 0, "cannot read stdout pipe flags");
        let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        ensure!(result >= 0, "cannot make stdout pipe nonblocking");
        let start = Instant::now();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            ensure!(
                start.elapsed() < timeout,
                "nvidia-smi timed out after {} ms",
                timeout.as_millis()
            );
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    if let Some(status) = child.try_wait()? {
                        ensure!(status.success(), "nvidia-smi query failed: {status}");
                        return Ok(String::from_utf8(bytes)?);
                    }
                }
                Ok(length) => {
                    ensure!(
                        bytes.len() as u64 + length as u64 <= LIMIT,
                        "nvidia-smi output exceeds 4 MiB"
                    );
                    bytes.extend_from_slice(&buffer[..length]);
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn allocation(text: &str) -> Result<u64> {
    let (number, scale) = match text.as_bytes().last() {
        Some(b'K') => (&text[..text.len() - 1], 1024),
        Some(b'M') => (&text[..text.len() - 1], MIB),
        Some(b'G') => (&text[..text.len() - 1], 1024 * MIB),
        _ => (text, 1),
    };
    number
        .parse::<u64>()?
        .checked_mul(scale)
        .context("GPU allocation overflows bytes")
}

fn parse_nvmap(text: &str) -> Result<(u64, BTreeMap<u32, u64>)> {
    let mut header = false;
    let mut total = None;
    let mut processes = BTreeMap::<u32, u64>::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields == ["CLIENT", "PROCESS", "PID", "SIZE"] {
            header = true;
            continue;
        }
        ensure!(header, "unrecognized nvmap header");
        if fields[0].eq_ignore_ascii_case("total") {
            ensure!(fields.len() == 2 && total.is_none(), "invalid nvmap total");
            total = Some(allocation(fields[1])?);
        } else {
            ensure!(fields.len() >= 4, "invalid nvmap client row");
            let pid: u32 = fields[fields.len() - 2].parse()?;
            let bytes = allocation(fields[fields.len() - 1])?;
            let entry = processes.entry(pid).or_default();
            *entry = entry
                .checked_add(bytes)
                .context("nvmap process allocation overflow")?;
        }
    }
    Ok((total.context("nvmap total missing")?, processes))
}

fn unsupported(text: &str) -> bool {
    matches!(
        text.trim(),
        "N/A" | "[N/A]" | "-" | "Not Supported" | "[Not Supported]"
    )
}
fn percent(text: &str) -> Result<Option<f64>> {
    if unsupported(text) {
        return Ok(None);
    }
    let value: f64 = text.trim().parse()?;
    ensure!(
        value.is_finite() && (0.0..=100.0).contains(&value),
        "invalid GPU percent"
    );
    Ok(Some(value))
}
fn mib(text: &str) -> Result<Option<u64>> {
    if unsupported(text) {
        return Ok(None);
    }
    Ok(Some(
        text.trim()
            .parse::<u64>()?
            .checked_mul(MIB)
            .context("GPU memory overflow")?,
    ))
}
fn parse_device(text: &str) -> Result<GpuMetricsData> {
    let rows: Vec<_> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    ensure!(rows.len() == 1, "expected one nvidia-smi device");
    let fields: Vec<_> = rows[0].split(',').map(str::trim).collect();
    ensure!(fields.len() == 4, "invalid nvidia-smi device row");
    ensure!(
        !fields[0].is_empty() && !unsupported(fields[0]) && !fields[0].contains("nvgpu"),
        "Orin nvidia-smi stub is not a GPU accounting source"
    );
    let utilization = percent(fields[1])?;
    let total = mib(fields[2])?;
    let used = mib(fields[3])?;
    if let (Some(total), Some(used)) = (total, used) {
        ensure!(used <= total, "GPU memory used exceeds total");
    }
    let now = timestamp_ns();
    Ok(GpuMetricsData {
        utilization_percent: utilization,
        memory_used_bytes: used,
        memory_total_bytes: total,
        utilization_source: "nvidia-smi/utilization.gpu".into(),
        memory_source: "nvidia-smi/memory.used,memory.total".into(),
        memory_scope: "device_gpu_allocation; shared physical RAM on Jetson".into(),
        utilization_timestamp_ns: utilization.map(|_| now),
        memory_timestamp_ns: (used.is_some() || total.is_some()).then_some(now),
        utilization_error: utilization
            .is_none()
            .then(|| "nvidia-smi reports system utilization N/A".into()),
        memory_error: (used.is_none() || total.is_none())
            .then(|| "nvidia-smi reports memory.used or memory.total N/A".into()),
    })
}
fn parse_process_memory(text: &str) -> Result<BTreeMap<u32, Option<u64>>> {
    let mut rows = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        ensure!(fields.len() == 2, "invalid nvidia-smi compute-apps row");
        let pid: u32 = fields[0].parse()?;
        ensure!(
            pid > 0 && !rows.contains_key(&pid),
            "invalid or duplicate compute PID"
        );
        rows.insert(pid, mib(fields[1])?);
    }
    // An empty compute table is valid: processes may start after the startup probe.
    Ok(rows)
}
fn parse_process_utilization(text: &str) -> Result<BTreeMap<u32, Option<f64>>> {
    let mut columns = None;
    let mut rows = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(comment) = line.trim().strip_prefix('#') {
            let fields: Vec<_> = comment.split_whitespace().collect();
            if let (Some(pid), Some(sm)) = (
                fields.iter().position(|s| *s == "pid"),
                fields.iter().position(|s| *s == "sm"),
            ) {
                ensure!(fields.first() == Some(&"gpu"), "unrecognized pmon header");
                columns = Some((pid, sm));
            }
            continue;
        }
        let (pid, sm) = columns.context("pmon header missing or unsupported")?;
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(
            fields.len() > pid.max(sm) && fields[0] == "0",
            "invalid pmon row"
        );
        if fields[pid] == "-" {
            continue;
        }
        let pid: u32 = fields[pid].parse()?;
        ensure!(
            pid > 0 && !rows.contains_key(&pid),
            "invalid or duplicate pmon PID"
        );
        rows.insert(pid, percent(fields[sm])?);
    }
    if columns.is_none() {
        bail!("pmon is unavailable or has no recognized header");
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::procfs::fake_stat;

    #[test]
    fn nvmap_units_names_and_reported_total_are_preserved() {
        let (total, rows) = parse_nvmap("CLIENT PROCESS PID SIZE\nuser name with spaces 42 2K\nuser same-pid 42 1M\nuser other 43 1G\ntotal 7M\n").unwrap();
        assert_eq!(total, 7 * MIB);
        assert_eq!(rows[&42], 2 * 1024 + MIB);
        assert_eq!(rows[&43], 1024 * MIB);
        assert_ne!(total, rows.values().sum::<u64>());
        for invalid in [
            "",
            "total 10K",
            "CLIENT PROCESS PID SIZE\nuser x 42 10K",
            "CLIENT PROCESS PID SIZE\ntotal invalid",
            "CLIENT PROCESS PID SIZE\ntotal 18446744073709551615G",
        ] {
            assert!(parse_nvmap(invalid).is_err());
        }
    }

    #[test]
    fn smi_distinguishes_missing_zero_and_unsupported_sources() {
        let system = parse_device("NVIDIA Thor, 42, [N/A], [N/A]\n").unwrap();
        assert_eq!(system.utilization_percent, Some(42.0));
        assert!(system.memory_used_bytes.is_none());
        assert!(system.memory_error.is_some());
        assert!(parse_device("Orin (nvgpu), 0, N/A, N/A").is_err());
        assert!(parse_device("Thor, NaN, 100, 50").is_err());
        assert!(parse_device("Thor, 101, 100, 50").is_err());
        let rows = parse_process_memory("42, 0\n43, [N/A]\n44, 16").unwrap();
        assert_eq!(rows[&42], Some(0));
        assert_eq!(rows[&43], None);
        assert_eq!(rows[&44], Some(16 * MIB));
        let rows = parse_process_utilization("# gpu pid type sm mem enc dec command\n# Idx # C/G % % % % name\n0 42 C 0 - - - name with spaces\n0 43 C - - - - name\n").unwrap();
        assert_eq!(rows[&42], Some(0.0));
        assert_eq!(rows[&43], None);
        assert!(parse_process_utilization("Not Supported").is_err());
        assert!(
            parse_process_utilization("# gpu pid type sm mem command\n0 - - - - -\n")
                .unwrap()
                .is_empty()
        );
        assert!(probe_values::<f64>(BTreeMap::new()).is_ok());
        assert!(probe_values(BTreeMap::from([(42, None::<f64>)])).is_err());
    }

    fn fixture_process(root: &Path, start: u64) {
        fs::create_dir_all(root.join("42")).unwrap();
        fs::write(root.join("42/stat"), fake_stat(42, "compute", 0, start)).unwrap();
    }
    fn selector() -> Option<Selector> {
        Some(Selector {
            pids: vec![42],
            names: vec![],
        })
    }
    #[cfg(unix)]
    fn script(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn nvmap_collects_fresh_process_allocations_and_never_sums_them_as_system_usage() {
        let dir = tempfile::tempdir().unwrap();
        fixture_process(dir.path(), 7);
        let path = dir.path().join("clients");
        fs::write(
            &path,
            "CLIENT PROCESS PID SIZE\nuser compute 42 1K\ntotal 2K",
        )
        .unwrap();
        let mut collector = GpuAccountingCollector::probe(
            path.clone(),
            dir.path().join("missing-smi"),
            Duration::from_secs(1),
            dir.path().into(),
            selector(),
        )
        .unwrap();
        fs::write(
            &path,
            "CLIENT PROCESS PID SIZE\nuser compute 42 3K\nuser ignored 43 9K\ntotal 10K",
        )
        .unwrap();
        let sample = collector.collect(&CollectContext::now()).unwrap();
        assert_eq!(sample.system.memory_used_bytes, Some(10 * 1024));
        assert_eq!(sample.processes.len(), 1);
        let process = &sample.processes[&(42, 7)];
        assert_eq!(process.memory_used_bytes, Some(3 * 1024));
        assert!(process.utilization_percent.is_none());
        assert!(process.utilization_error.as_ref().unwrap().contains("Orin"));
        fs::write(&path, "invalid").unwrap();
        assert!(collector.collect(&CollectContext::now()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn thor_capabilities_are_probed_once_and_pid_reuse_during_queries_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fixture_process(dir.path(), 7);
        let executable = dir.path().join("nvidia-smi");
        script(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$0.calls\"\ncase \"$1\" in\n --query-gpu=*) echo 'Thor, 42, N/A, N/A';;\n --query-compute-apps=*)\n if [ -f \"$0.newstat\" ]; then mv \"$0.newstat\" \"${0%/*}/42/stat\"; fi\n echo '42, 16';;\n pmon) exit 3;;\n *) exit 9;;\nesac\n",
        );
        let collectors = GpuAccountingCollector::probe(
            dir.path().join("no-nvmap"),
            executable.clone(),
            Duration::from_secs(1),
            dir.path().into(),
            selector(),
        )
        .unwrap()
        .split();
        assert!(collectors.process_utilization.is_err());
        let mut load = collectors.system_utilization.unwrap();
        let mut collector = collectors.memory.unwrap();
        for _ in 0..3 {
            let sample = collector.collect(&CollectContext::now()).unwrap();
            assert_eq!(
                load.collect(&CollectContext::now())
                    .unwrap()
                    .utilization_percent,
                42.0
            );
            assert!(sample.system.utilization_percent.is_none());
            assert!(sample.system.memory_used_bytes.is_none());
            let process = &sample.processes[&(42, 7)];
            assert_eq!(process.memory_used_bytes, Some(16 * MIB));
            assert!(process.utilization_percent.is_none());
            assert!(process.utilization_error.is_some());
        }
        let calls = fs::read_to_string(executable.with_extension("calls")).unwrap();
        assert_eq!(calls.lines().filter(|line| *line == "pmon").count(), 1);
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.starts_with("--query-compute-apps"))
                .count(),
            4
        );
        fs::write(
            executable.with_extension("newstat"),
            fake_stat(42, "new", 0, 8),
        )
        .unwrap();
        let sample = collector.collect(&CollectContext::now()).unwrap();
        assert!(
            sample.processes.is_empty(),
            "PID changed during the driver query"
        );
        let sample = collector.collect(&CollectContext::now()).unwrap();
        assert!(sample.processes.contains_key(&(42, 8)));
    }

    #[cfg(unix)]
    #[test]
    fn thor_keeps_process_and_system_utilization_separate_and_queries_time_out() {
        let dir = tempfile::tempdir().unwrap();
        fixture_process(dir.path(), 7);
        let executable = dir.path().join("nvidia-smi");
        script(
            &executable,
            "#!/bin/sh\ncase \"$1\" in\n --query-gpu=*) echo 'Thor, 42, 1000, 200';;\n --query-compute-apps=*) echo '42, 16';;\n pmon) printf '%s\\n' '# gpu pid type sm mem command' '0 42 C 8 - compute';;\nesac\n",
        );
        let collectors = GpuAccountingCollector::probe(
            dir.path().join("no-nvmap"),
            executable.clone(),
            Duration::from_secs(1),
            dir.path().into(),
            selector(),
        )
        .unwrap()
        .split();
        let mut memory = collectors.memory.unwrap();
        let mut utilization = collectors.process_utilization.unwrap();
        let mut load = collectors.system_utilization.unwrap();
        assert_eq!(
            load.collect(&CollectContext::now())
                .unwrap()
                .utilization_percent,
            42.0
        );
        let sample = memory.collect(&CollectContext::now()).unwrap();
        assert_eq!(sample.system.memory_used_bytes, Some(200 * MIB));
        assert!(sample.processes[&(42, 7)].utilization_percent.is_none());
        let sample = utilization.collect(&CollectContext::now()).unwrap();
        assert_eq!(sample.processes[&(42, 7)].utilization_percent, Some(8.0));
        assert!(sample.system.memory_used_bytes.is_none());
        script(&executable, "#!/bin/sh\nexec sleep 30\n");
        assert!(
            command(&executable, DEVICE_QUERY, Duration::from_millis(50))
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
    }
}
