use crate::{
    data::{CpuData, GpuAccountingData, GpuData, GpuMetricsData, MemoryData, Processes, Threads},
    framework::{Assembler, Batch, Key, Sample, StreamDescriptor},
    protocol::{self, pb},
};
use anyhow::Result;
use std::{collections::HashMap, sync::Arc};

#[derive(Clone)]
pub struct Identity {
    pub host: String,
    pub boot_id: String,
}
impl Identity {
    fn header(&self, batch: &Batch, dependencies: &[&str]) -> pb::Header {
        pb::Header {
            timestamp: Some(protocol::timestamp(batch.captured_at_ns)),
            batch_id: batch.id,
            host: self.host.clone(),
            boot_id: self.boot_id.clone(),
            inputs: dependencies
                .iter()
                .map(|name| {
                    let (time, error) = batch.status(name);
                    pb::InputStatus {
                        collector: (*name).into(),
                        timestamp: Some(protocol::timestamp(time)),
                        error,
                    }
                })
                .collect(),
        }
    }
    fn process_id(&self, pid: u32, start: u64) -> String {
        format!("{}:{pid}:{start}", self.boot_id)
    }
}
fn memory(data: &MemoryData) -> pb::Memory {
    pb::Memory {
        total_bytes: data.total_bytes,
        available_bytes: data.available_bytes,
        used_bytes: data.total_bytes.saturating_sub(data.available_bytes),
        swap_total_bytes: data.swap_total_bytes,
        swap_used_bytes: data.swap_total_bytes.saturating_sub(data.swap_free_bytes),
    }
}

fn gpu_metrics(data: &GpuMetricsData) -> pb::GpuMetrics {
    pb::GpuMetrics {
        utilization_percent: data.utilization_percent,
        memory_used_bytes: data.memory_used_bytes,
        memory_total_bytes: data.memory_total_bytes,
        utilization_source: data.utilization_source.clone(),
        memory_source: data.memory_source.clone(),
        memory_scope: data.memory_scope.clone(),
        utilization_timestamp: data.utilization_timestamp_ns.map(protocol::timestamp),
        memory_timestamp: data.memory_timestamp_ns.map(protocol::timestamp),
        utilization_error: data.utilization_error.clone(),
        memory_error: data.memory_error.clone(),
    }
}

#[derive(Clone, Default)]
pub struct GpuInputs {
    pub utilization: Option<Key<GpuData>>,
    pub accounting: Option<Key<GpuAccountingData>>,
    pub utilization_error: Option<String>,
    pub accounting_error: Option<String>,
}
impl GpuInputs {
    fn system(&self, batch: &Batch) -> pb::GpuMetrics {
        let mut data = self
            .accounting
            .and_then(|key| batch.get(key).ok())
            .map(|data| data.system.clone())
            .unwrap_or_else(|| GpuMetricsData {
                utilization_error: self.utilization_error.clone(),
                memory_error: self
                    .accounting_error
                    .clone()
                    .or_else(|| self.accounting.and_then(|key| batch.status(key.name).1)),
                ..Default::default()
            });
        if let Some(key) = self.utilization {
            match batch.get(key) {
                Ok(usage) => {
                    data.utilization_percent = Some(usage.utilization_percent);
                    data.utilization_source = usage.source.clone();
                    data.utilization_timestamp_ns = Some(usage.observed_at_ns);
                    data.utilization_error = None;
                }
                Err(error) if data.utilization_percent.is_none() => {
                    data.utilization_error = Some(format!("{error:#}"));
                }
                Err(_) => {}
            }
        } else if data.utilization_percent.is_none() {
            data.utilization_error = self.utilization_error.clone().or(data.utilization_error);
        }
        gpu_metrics(&data)
    }
    fn process(&self, batch: &Batch, pid: u32, start: u64) -> pb::GpuMetrics {
        if let Some(data) = self.accounting.and_then(|key| batch.get(key).ok()) {
            return gpu_metrics(
                data.processes
                    .get(&(pid, start))
                    .unwrap_or(&data.missing_process),
            );
        }
        let error = self
            .accounting_error
            .clone()
            .or_else(|| self.accounting.and_then(|key| batch.status(key.name).1))
            .unwrap_or_else(|| "GPU process metrics unavailable".into());
        pb::GpuMetrics {
            utilization_error: Some(error.clone()),
            memory_error: Some(error),
            ..Default::default()
        }
    }
}

pub struct SystemAssembler {
    cpu: Key<CpuData>,
    memory: Key<MemoryData>,
    gpu: Option<GpuInputs>,
    dependencies: Vec<&'static str>,
    identity: Identity,
    stream: Arc<StreamDescriptor>,
}
impl SystemAssembler {
    pub fn new(
        cpu: Key<CpuData>,
        memory: Key<MemoryData>,
        gpu: Option<GpuInputs>,
        identity: Identity,
    ) -> Self {
        let mut dependencies = vec![cpu.name, memory.name];
        if let Some(gpu) = &gpu {
            dependencies.extend(gpu.utilization.map(|key| key.name));
            dependencies.extend(gpu.accounting.map(|key| key.name));
        }
        Self {
            cpu,
            memory,
            gpu,
            dependencies,
            identity,
            stream: protocol::stream("/syslens/system", "SystemSample"),
        }
    }
}
impl Assembler for SystemAssembler {
    fn name(&self) -> &str {
        "system"
    }
    fn dependencies(&self) -> &[&'static str] {
        &self.dependencies
    }
    fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>> {
        let message = pb::SystemSample {
            header: Some(self.identity.header(batch, &self.dependencies)),
            cpu: batch.get(self.cpu).ok().map(|data| pb::Cpu {
                usage_percent: data.usage_percent,
                cores: data
                    .cores
                    .iter()
                    .map(|(id, percent)| pb::CpuCore {
                        core_id: *id,
                        usage_percent: *percent,
                    })
                    .collect(),
            }),
            memory: batch.get(self.memory).ok().map(memory),
            gpu: self.gpu.as_ref().map(|gpu| gpu.system(batch)),
        };
        Ok(vec![protocol::encode(
            &message,
            self.stream.clone(),
            batch.captured_at_ns,
        )])
    }
}

pub struct ProcessAssembler {
    processes: Key<Processes>,
    threads: Option<Key<Threads>>,
    gpu: Option<GpuInputs>,
    dependencies: Vec<&'static str>,
    identity: Identity,
    stream: Arc<StreamDescriptor>,
}
impl ProcessAssembler {
    pub fn new(
        processes: Key<Processes>,
        threads: Option<Key<Threads>>,
        gpu: Option<GpuInputs>,
        identity: Identity,
    ) -> Self {
        let mut dependencies = vec![processes.name];
        if let Some(key) = threads {
            dependencies.push(key.name);
        }
        if let Some(key) = gpu.as_ref().and_then(|gpu| gpu.accounting) {
            dependencies.push(key.name);
        }
        Self {
            processes,
            threads,
            gpu,
            dependencies,
            identity,
            stream: protocol::stream("/syslens/processes", "ProcessSample"),
        }
    }
}
impl Assembler for ProcessAssembler {
    fn name(&self) -> &str {
        "processes"
    }
    fn dependencies(&self) -> &[&'static str] {
        &self.dependencies
    }
    fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>> {
        let processes = batch.get(self.processes).ok();
        let threads = self.threads.and_then(|key| batch.get(key).ok());
        let groups: HashMap<_, _> = threads
            .into_iter()
            .flat_map(|data| &data.groups)
            .map(|group| ((group.pid, group.process_start_ticks), group))
            .collect();
        let items = processes
            .into_iter()
            .flat_map(|data| &data.items)
            .map(|process| {
                let id = self.identity.process_id(process.pid, process.start_ticks);
                let group = groups.get(&(process.pid, process.start_ticks));
                let threads_error = if self.threads.is_some() {
                    match group {
                        Some(group) => group.error.clone(),
                        None => {
                            Some("no matching thread snapshot for this process instance".into())
                        }
                    }
                } else {
                    None
                };
                pb::Process {
                    pid: process.pid,
                    start_ticks: process.start_ticks,
                    instance_id: id.clone(),
                    name: process.name.clone(),
                    cpu_percent: process.cpu_percent,
                    rss_bytes: process.rss_bytes,
                    virtual_bytes: process.virtual_bytes,
                    cpu_time_ticks: process.cpu_ticks,
                    threads: group
                        .into_iter()
                        .flat_map(|group| &group.items)
                        .map(|thread| pb::Thread {
                            tid: thread.tid,
                            start_ticks: thread.start_ticks,
                            instance_id: format!("{id}:{}:{}", thread.tid, thread.start_ticks),
                            name: thread.name.clone(),
                            cpu_percent: thread.cpu_percent,
                            cpu_time_ticks: thread.cpu_ticks,
                        })
                        .collect(),
                    threads_error,
                    gpu: self
                        .gpu
                        .as_ref()
                        .map(|gpu| gpu.process(batch, process.pid, process.start_ticks)),
                }
            })
            .collect();
        let message = pb::ProcessSample {
            header: Some(self.identity.header(batch, &self.dependencies)),
            processes: items,
            skipped_processes: processes.map_or(0, |data| data.skipped),
        };
        Ok(vec![protocol::encode(
            &message,
            self.stream.clone(),
            batch.captured_at_ns,
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    fn identity() -> Identity {
        Identity {
            host: "test".into(),
            boot_id: "boot".into(),
        }
    }
    #[test]
    fn combines_system_collectors_without_stale_data_or_substituting_ram_for_gpu_memory() {
        let cpu = Key::new("cpu");
        let mem = Key::new("memory");
        let gpu = Key::new("gpu");
        let mut system = SystemAssembler::new(
            cpu,
            mem,
            Some(GpuInputs {
                utilization: Some(gpu),
                ..Default::default()
            }),
            identity(),
        );
        let mut batch = Batch::for_test(1);
        batch.insert(
            cpu,
            CpuData {
                usage_percent: Some(37.5),
                ..Default::default()
            },
        );
        batch.insert(
            mem,
            MemoryData {
                total_bytes: 4096,
                available_bytes: 1024,
                ..Default::default()
            },
        );
        batch.insert(
            gpu,
            GpuData {
                utilization_percent: 12.0,
                source: "test".into(),
                observed_at_ns: batch.captured_at_ns,
            },
        );
        let a =
            pb::SystemSample::decode(system.assemble(&batch).unwrap()[0].payload.clone()).unwrap();
        assert_eq!(a.cpu.unwrap().usage_percent, Some(37.5));
        assert_eq!(a.memory.unwrap().used_bytes, 3072);
        assert_eq!(a.gpu.as_ref().unwrap().utilization_percent, Some(12.0));
        assert!(a.gpu.unwrap().memory_used_bytes.is_none());
        let empty = pb::SystemSample::decode(
            system.assemble(&Batch::for_test(2)).unwrap()[0]
                .payload
                .clone(),
        )
        .unwrap();
        assert!(empty.cpu.is_none());
        assert!(empty.memory.is_none());
        assert!(empty.gpu.as_ref().unwrap().utilization_percent.is_none());
        assert!(
            empty
                .header
                .unwrap()
                .inputs
                .iter()
                .all(|input| input.error.is_some())
        );
    }
    #[test]
    fn never_joins_threads_or_gpu_data_from_a_reused_pid() {
        use crate::data::{ProcessData, ThreadGroup};
        let proc_key = Key::new("process");
        let thread_key = Key::new("threads");
        let gpu_key = Key::new("gpu_accounting");
        let gpu_inputs = GpuInputs {
            accounting: Some(gpu_key),
            ..Default::default()
        };
        let mut system = SystemAssembler::new(
            Key::new("cpu"),
            Key::new("memory"),
            Some(gpu_inputs.clone()),
            identity(),
        );
        let mut assembler =
            ProcessAssembler::new(proc_key, Some(thread_key), Some(gpu_inputs), identity());
        let mut batch = Batch::for_test(0);
        batch.insert(
            proc_key,
            Processes {
                items: vec![ProcessData {
                    pid: 42,
                    start_ticks: 9,
                    name: "new".into(),
                    cpu_percent: None,
                    cpu_ticks: 0,
                    rss_bytes: 0,
                    virtual_bytes: 0,
                }],
                skipped: 0,
            },
        );
        batch.insert(
            thread_key,
            Threads {
                groups: vec![ThreadGroup {
                    pid: 42,
                    process_start_ticks: 8,
                    items: Vec::new(),
                    error: None,
                }],
            },
        );
        let accounting = |start| GpuAccountingData {
            system: GpuMetricsData {
                utilization_percent: Some(42.0),
                memory_used_bytes: Some(8192),
                ..Default::default()
            },
            processes: [(
                (42, start),
                GpuMetricsData {
                    utilization_percent: Some(8.0),
                    memory_used_bytes: Some(1024),
                    ..Default::default()
                },
            )]
            .into(),
            missing_process: GpuMetricsData {
                memory_error: Some("no matching process observation".into()),
                utilization_error: Some("no matching process observation".into()),
                ..Default::default()
            },
        };
        batch.insert(gpu_key, accounting(8));
        let message =
            pb::ProcessSample::decode(assembler.assemble(&batch).unwrap()[0].payload.clone())
                .unwrap();
        assert!(message.processes[0].threads_error.is_some());
        assert_eq!(message.processes[0].instance_id, "boot:42:9");
        assert!(
            message.processes[0]
                .gpu
                .as_ref()
                .unwrap()
                .memory_used_bytes
                .is_none()
        );
        assert!(
            message.processes[0]
                .gpu
                .as_ref()
                .unwrap()
                .memory_error
                .is_some()
        );
        batch.insert(gpu_key, accounting(9));
        let message =
            pb::ProcessSample::decode(assembler.assemble(&batch).unwrap()[0].payload.clone())
                .unwrap();
        let process_gpu = message.processes[0].gpu.as_ref().unwrap();
        assert_eq!(process_gpu.utilization_percent, Some(8.0));
        assert_eq!(process_gpu.memory_used_bytes, Some(1024));
        let message =
            pb::SystemSample::decode(system.assemble(&batch).unwrap()[0].payload.clone()).unwrap();
        assert_eq!(
            message.gpu.as_ref().unwrap().utilization_percent,
            Some(42.0)
        );
        assert_eq!(message.gpu.unwrap().memory_used_bytes, Some(8192));
    }
}
