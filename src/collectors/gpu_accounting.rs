use super::{
    command::command,
    procfs::{Selector, read_stat},
};
use crate::{
    data::{GpuData, GpuMemory, GpuMemoryData, GpuUtilization, Named, ProcessGpuUtilizationData},
    framework::{Collector, OperationContext, timestamp_ns},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

const MIB: u64 = 1024 * 1024;
const LOAD_QUERY: &[&str] = &[
    "--query-gpu=name,utilization.gpu",
    "--format=csv,noheader,nounits",
    "-i",
    "0",
];
const MEMORY_QUERY: &[&str] = &[
    "--query-gpu=name,memory.total,memory.used",
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

pub type Capability<T> = Result<Box<dyn Collector<Data = T>>, String>;
pub struct AccountingSources {
    pub memory: Capability<GpuMemoryData>,
    pub process_utilization: Capability<ProcessGpuUtilizationData>,
    pub system_utilization: Capability<GpuData>,
}
#[derive(Clone)]
struct Query {
    executable: PathBuf,
    timeout: Duration,
}
impl Query {
    fn run(&self, args: &[&str], context: &OperationContext) -> Result<String> {
        command(&self.executable, args, &context.limited(self.timeout))
    }
}
#[derive(Clone)]
struct ProcessAccess {
    root: PathBuf,
    selector: Option<Selector>,
}
impl ProcessAccess {
    fn identities(&self, context: &OperationContext) -> Result<BTreeMap<u32, (u64, String)>> {
        let mut identities = BTreeMap::new();
        if let Some(selector) = &self.selector {
            for pid in selector.candidates(&self.root)? {
                context.check()?;
                if let Ok(stat) = read_stat(&self.root.join(pid.to_string()).join("stat"))
                    && stat.pid == pid
                    && selector.includes(&stat)
                {
                    identities.insert(pid, (stat.start_ticks, stat.name));
                }
            }
        }
        Ok(identities)
    }
    fn join<T>(
        &self,
        before: BTreeMap<u32, (u64, String)>,
        rows: BTreeMap<u32, T>,
        context: &OperationContext,
    ) -> Result<BTreeMap<(u32, u64), Named<T>>> {
        let after = self.identities(context)?;
        Ok(rows
            .into_iter()
            .filter_map(|(pid, value)| {
                let (start, _) = before.get(&pid)?;
                let (current, name) = after.get(&pid)?;
                (start == current).then_some((
                    (pid, *start),
                    Named {
                        name: name.clone(),
                        value,
                    },
                ))
            })
            .collect())
    }
}

impl AccountingSources {
    pub fn probe(
        nvmap: PathBuf,
        executable: PathBuf,
        timeout: Duration,
        proc_root: PathBuf,
        selector: Option<Selector>,
        want_system_utilization: bool,
        context: &OperationContext,
    ) -> Self {
        let query = Query {
            executable,
            timeout,
        };
        let access = ProcessAccess {
            root: proc_root,
            selector,
        };
        // No common device query gates other capabilities. Each source gets one
        // independent probe, and failed capabilities are never polled later.
        let system_utilization = if want_system_utilization {
            query
                .run(LOAD_QUERY, context)
                .and_then(|text| parse_load(&text))
                .map(|_| {
                    Box::new(SmiLoad {
                        query: query.clone(),
                    }) as Box<dyn Collector<Data = GpuData>>
                })
                .map_err(|e| format!("{e:#}"))
        } else {
            Err("system utilization provided by another source".into())
        };
        let process_utilization = if access.selector.is_some() {
            query
                .run(UTILIZATION_QUERY, context)
                .and_then(|text| parse_process_utilization(&text))
                .and_then(probe_values)
                .map(|_| {
                    Box::new(SmiProcessUtilization {
                        query: query.clone(),
                        access: access.clone(),
                    }) as Box<dyn Collector<Data = ProcessGpuUtilizationData>>
                })
                .map_err(|e| format!("{e:#}"))
        } else {
            Err("process collection disabled".into())
        };
        let memory = match fs::read_to_string(&nvmap)
            .with_context(|| nvmap.display().to_string())
            .and_then(|text| parse_nvmap(&text))
        {
            Ok(_) => Ok(Box::new(MemoryCollector {
                backend: MemoryBackend::Nvmap(nvmap),
                access,
            }) as Box<dyn Collector<Data = GpuMemoryData>>),
            Err(nvmap_error) => {
                let device = query
                    .run(MEMORY_QUERY, context)
                    .and_then(|text| parse_memory(&text))
                    .and_then(|data| {
                        ensure!(
                            data.used_bytes.is_some() || data.total_bytes.is_some(),
                            "system GPU memory unavailable"
                        );
                        Ok(())
                    })
                    .map_err(|e| format!("{e:#}"));
                let processes = if access.selector.is_some() {
                    query
                        .run(PROCESS_QUERY, context)
                        .and_then(|text| parse_process_memory(&text))
                        .and_then(probe_values)
                        .map_err(|e| format!("{e:#}"))
                } else {
                    Err("process collection disabled".into())
                };
                match (device, processes) {
                    (Err(device), Err(processes)) => Err(format!(
                        "nvmap: {nvmap_error:#}; system memory: {device}; process memory: {processes}"
                    )),
                    (device, processes) => Ok(Box::new(MemoryCollector {
                        backend: MemoryBackend::Smi {
                            query,
                            device,
                            processes,
                        },
                        access,
                    })
                        as Box<dyn Collector<Data = GpuMemoryData>>),
                }
            }
        };
        Self {
            memory,
            process_utilization,
            system_utilization,
        }
    }
}
struct SmiLoad {
    query: Query,
}
impl Collector for SmiLoad {
    type Data = GpuData;
    fn collect(&mut self, context: &OperationContext) -> Result<Self::Data> {
        parse_load(&self.query.run(LOAD_QUERY, context)?)
    }
}
enum MemoryBackend {
    Nvmap(PathBuf),
    Smi {
        query: Query,
        device: Result<(), String>,
        processes: Result<(), String>,
    },
}
struct MemoryCollector {
    backend: MemoryBackend,
    access: ProcessAccess,
}
impl Collector for MemoryCollector {
    type Data = GpuMemoryData;
    fn collect(&mut self, context: &OperationContext) -> Result<Self::Data> {
        context.check()?;
        let before = self.access.identities(context)?;
        let (system, rows, missing_process) = match &self.backend {
            MemoryBackend::Nvmap(path) => {
                let (total, clients) = parse_nvmap(
                    &fs::read_to_string(path).with_context(|| path.display().to_string())?,
                )?;
                let now = timestamp_ns();
                let missing = GpuMemory {
                    source: path.display().to_string(),
                    scope: "nvmap_client_allocations; may include multimedia/shared allocations"
                        .into(),
                    error: Some("process is absent from nvmap snapshot".into()),
                    ..Default::default()
                };
                let rows = clients
                    .into_iter()
                    .map(|(pid, used)| {
                        (
                            pid,
                            GpuMemory {
                                used_bytes: Some(used),
                                observed_at_ns: Some(now),
                                error: None,
                                ..missing.clone()
                            },
                        )
                    })
                    .collect();
                (
                    GpuMemory {
                        used_bytes: Some(total),
                        source: path.display().to_string(),
                        scope:
                            "nvmap_total_allocations; not GPU-exclusive or a sum of process rows"
                                .into(),
                        observed_at_ns: Some(now),
                        ..Default::default()
                    },
                    rows,
                    missing,
                )
            }
            MemoryBackend::Smi {
                query,
                device,
                processes,
            } => {
                let system = available_query(device, query, MEMORY_QUERY, context)
                    .and_then(|text| parse_memory(&text))
                    .unwrap_or_else(|error| GpuMemory {
                        error: Some(format!("{error:#}")),
                        ..Default::default()
                    });
                let result = available_query(processes, query, PROCESS_QUERY, context)
                    .and_then(|text| parse_process_memory(&text));
                let missing = GpuMemory {
                    source: "nvidia-smi/compute-apps/used_memory".into(),
                    scope:
                        "compute_process_gpu_allocation; graphics-only processes are not covered"
                            .into(),
                    error: Some(match &result {
                        Ok(_) => "process is absent from compute-apps snapshot".into(),
                        Err(e) => format!("{e:#}"),
                    }),
                    ..Default::default()
                };
                let now = timestamp_ns();
                let rows = result
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(pid, bytes)| {
                        (
                            pid,
                            GpuMemory {
                                used_bytes: bytes,
                                observed_at_ns: bytes.map(|_| now),
                                error: bytes
                                    .is_none()
                                    .then(|| "nvidia-smi process memory N/A".into()),
                                ..missing.clone()
                            },
                        )
                    })
                    .collect();
                (system, rows, missing)
            }
        };
        Ok(GpuMemoryData {
            system,
            processes: self.access.join(before, rows, context)?,
            missing_process,
        })
    }
}
struct SmiProcessUtilization {
    query: Query,
    access: ProcessAccess,
}
impl Collector for SmiProcessUtilization {
    type Data = ProcessGpuUtilizationData;
    fn collect(&mut self, context: &OperationContext) -> Result<Self::Data> {
        let before = self.access.identities(context)?;
        let values = parse_process_utilization(&self.query.run(UTILIZATION_QUERY, context)?)?;
        let missing_process = GpuUtilization {
            source: "nvidia-smi/pmon/sm".into(),
            error: Some("process is absent from pmon snapshot".into()),
            ..Default::default()
        };
        let now = timestamp_ns();
        let rows = values
            .into_iter()
            .map(|(pid, percent)| {
                (
                    pid,
                    GpuUtilization {
                        percent,
                        observed_at_ns: percent.map(|_| now),
                        error: percent
                            .is_none()
                            .then(|| "nvidia-smi process utilization N/A".into()),
                        ..missing_process.clone()
                    },
                )
            })
            .collect();
        Ok(ProcessGpuUtilizationData {
            processes: self.access.join(before, rows, context)?,
            missing_process,
        })
    }
}
fn available_query(
    capability: &Result<(), String>,
    query: &Query,
    args: &[&str],
    context: &OperationContext,
) -> Result<String> {
    capability
        .as_ref()
        .map_err(|error| anyhow!(error.clone()))?;
    query.run(args, context)
}
fn probe_values<T>(rows: BTreeMap<u32, Option<T>>) -> Result<()> {
    ensure!(
        rows.is_empty() || rows.values().any(Option::is_some),
        "all active process values are N/A"
    );
    Ok(())
}
fn device_fields(text: &str, count: usize) -> Result<Vec<&str>> {
    let rows: Vec<_> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    ensure!(rows.len() == 1, "expected one nvidia-smi device");
    let fields: Vec<_> = rows[0].split(',').map(str::trim).collect();
    ensure!(fields.len() == count, "invalid nvidia-smi device row");
    ensure!(
        !fields[0].is_empty() && !unsupported(fields[0]) && !fields[0].contains("nvgpu"),
        "Orin nvidia-smi stub is not a GPU source"
    );
    Ok(fields)
}
fn parse_load(text: &str) -> Result<GpuData> {
    let fields = device_fields(text, 2)?;
    Ok(GpuData {
        utilization_percent: percent(fields[1])?.context("system GPU utilization N/A")?,
        source: "nvidia-smi/utilization.gpu".into(),
        observed_at_ns: timestamp_ns(),
    })
}
fn parse_memory(text: &str) -> Result<GpuMemory> {
    let fields = device_fields(text, 3)?;
    let total_bytes = mib(fields[1])?;
    let used_bytes = mib(fields[2])?;
    if let (Some(total), Some(used)) = (total_bytes, used_bytes) {
        ensure!(used <= total, "GPU memory used exceeds total");
    }
    Ok(GpuMemory {
        total_bytes,
        used_bytes,
        source: "nvidia-smi/memory.used,memory.total".into(),
        scope: "device_gpu_allocation; shared physical RAM on Jetson".into(),
        observed_at_ns: (used_bytes.is_some() || total_bytes.is_some()).then(timestamp_ns),
        error: (used_bytes.is_none() || total_bytes.is_none())
            .then(|| "nvidia-smi memory N/A".into()),
    })
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
    use std::{os::unix::fs::PermissionsExt, path::Path};
    fn script(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn fixture(root: &Path, start: u64) {
        fs::create_dir_all(root.join("42")).unwrap();
        fs::write(
            root.join("42/stat"),
            super::super::procfs::fake_stat(42, "test", 1, start),
        )
        .unwrap();
    }
    fn selector() -> Option<Selector> {
        Some(Selector {
            pids: vec![42],
            names: vec![],
        })
    }
    #[test]
    fn parsers_preserve_units_scopes_and_missing_values() {
        let (total, rows) =
            parse_nvmap("CLIENT PROCESS PID SIZE\nuser test 42 16K\ntotal 32K\n").unwrap();
        assert_eq!(total, 32768);
        assert_eq!(rows[&42], 16384);
        assert_eq!(parse_load("Thor, 0").unwrap().utilization_percent, 0.0);
        assert!(parse_load("nvgpu, 0").is_err());
        assert!(parse_load("Thor, N/A").is_err());
        assert!(parse_memory("Thor, 100, 101").is_err());
        let memory = parse_memory("Thor, N/A, 10").unwrap();
        assert!(memory.total_bytes.is_none());
        assert_eq!(memory.used_bytes, Some(10 * MIB));
        assert_eq!(parse_process_memory("42, N/A").unwrap()[&42], None);
        assert!(probe_values::<u64>([(42, None)].into()).is_err());
        assert!(probe_values::<u64>(BTreeMap::new()).is_ok());
    }
    #[test]
    fn system_probe_failure_does_not_gate_process_capabilities_and_failures_are_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), 7);
        let smi = dir.path().join("smi");
        script(
            &smi,
            "#!/bin/sh\necho \"$1\" >> \"$0.calls\"\ncase \"$1\" in\n --query-gpu=*) exit 1;;\n --query-compute-apps=*) echo '42, 12';;\n pmon) printf '# gpu pid type sm mem enc dec command\n0 42 C 25 - - - test\n';;\nesac\n",
        );
        let mut sources = AccountingSources::probe(
            dir.path().join("missing"),
            smi.clone(),
            Duration::from_millis(100),
            dir.path().into(),
            selector(),
            true,
            &OperationContext::now(),
        );
        assert!(sources.system_utilization.is_err());
        assert!(sources.memory.is_ok());
        assert!(sources.process_utilization.is_ok());
        for _ in 0..3 {
            let memory = sources
                .memory
                .as_mut()
                .unwrap()
                .collect(&OperationContext::now())
                .unwrap();
            assert!(memory.system.used_bytes.is_none());
            assert_eq!(memory.processes[&(42, 7)].value.used_bytes, Some(12 * MIB));
            assert_eq!(memory.processes[&(42, 7)].name, "test");
            let usage = sources
                .process_utilization
                .as_mut()
                .unwrap()
                .collect(&OperationContext::now())
                .unwrap();
            assert_eq!(usage.processes[&(42, 7)].value.percent, Some(25.0));
        }
        let calls = fs::read_to_string(smi.with_extension("calls")).unwrap();
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.starts_with("--query-gpu="))
                .count(),
            2
        );
    }
    #[test]
    fn pid_reuse_during_query_never_inherits_gpu_values() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), 7);
        let smi = dir.path().join("smi");
        script(
            &smi,
            "#!/bin/sh\ncase \"$1\" in\n --query-gpu=name,memory*) echo 'Thor, 100, 12';;\n --query-gpu=*) echo 'Thor, 10';;\n --query-compute-apps=*) echo '42, 12';;\n pmon) printf '# gpu pid type sm mem enc dec command\n0 42 C 25 - - - test\n';;\nesac\n",
        );
        let mut sources = AccountingSources::probe(
            dir.path().join("missing"),
            smi.clone(),
            Duration::from_millis(200),
            dir.path().into(),
            selector(),
            true,
            &OperationContext::now(),
        );
        let next_stat = super::super::procfs::fake_stat(42, "replacement", 2, 9);
        script(
            &smi,
            &format!(
                "#!/bin/sh\nprintf '%s' '{}' > '{}'/42/stat\nprintf '# gpu pid type sm mem enc dec command\\n0 42 C 25 - - - test\\n'\n",
                next_stat,
                dir.path().display()
            ),
        );
        let sample = sources
            .process_utilization
            .as_mut()
            .unwrap()
            .collect(&OperationContext::now())
            .unwrap();
        assert!(sample.processes.is_empty());
    }
    #[test]
    fn nvmap_system_total_is_not_sum_of_process_rows() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), 7);
        let clients = dir.path().join("clients");
        fs::write(
            &clients,
            "CLIENT PROCESS PID SIZE\nuser test 42 16K\ntotal 64K\n",
        )
        .unwrap();
        let mut sources = AccountingSources::probe(
            clients,
            dir.path().join("missing-smi"),
            Duration::from_millis(100),
            dir.path().into(),
            selector(),
            true,
            &OperationContext::now(),
        );
        let data = sources
            .memory
            .as_mut()
            .unwrap()
            .collect(&OperationContext::now())
            .unwrap();
        assert_eq!(data.system.used_bytes, Some(65536));
        assert_eq!(data.processes[&(42, 7)].value.used_bytes, Some(16384));
        assert!(data.system.scope.contains("not GPU-exclusive"));
    }
}
