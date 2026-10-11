use crate::{
    data::{CpuData, GpuData, MemoryData, Processes, Threads},
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

pub struct SystemAssembler {
    cpu: Key<CpuData>,
    memory: Key<MemoryData>,
    dependencies: [&'static str; 2],
    identity: Identity,
    stream: Arc<StreamDescriptor>,
}
impl SystemAssembler {
    pub fn new(cpu: Key<CpuData>, memory: Key<MemoryData>, identity: Identity) -> Self {
        Self {
            cpu,
            memory,
            dependencies: [cpu.name, memory.name],
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
    dependencies: Vec<&'static str>,
    identity: Identity,
    stream: Arc<StreamDescriptor>,
}
impl ProcessAssembler {
    pub fn new(
        processes: Key<Processes>,
        threads: Option<Key<Threads>>,
        identity: Identity,
    ) -> Self {
        let mut dependencies = vec![processes.name];
        if let Some(key) = threads {
            dependencies.push(key.name);
        }
        Self {
            processes,
            threads,
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

pub struct GpuAssembler {
    gpu: Key<GpuData>,
    memory: Key<MemoryData>,
    dependencies: [&'static str; 2],
    identity: Identity,
    stream: Arc<StreamDescriptor>,
}
impl GpuAssembler {
    pub fn new(gpu: Key<GpuData>, memory: Key<MemoryData>, identity: Identity) -> Self {
        Self {
            gpu,
            memory,
            dependencies: [gpu.name, memory.name],
            identity,
            stream: protocol::stream("/syslens/gpu", "GpuSample"),
        }
    }
}
impl Assembler for GpuAssembler {
    fn name(&self) -> &str {
        "gpu"
    }
    fn dependencies(&self) -> &[&'static str] {
        &self.dependencies
    }
    fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>> {
        let gpu = batch.get(self.gpu).ok();
        let message = pb::GpuSample {
            header: Some(self.identity.header(batch, &self.dependencies)),
            device: "jetson-integrated".into(),
            utilization_percent: gpu.map(|data| data.utilization_percent),
            source: gpu.map(|data| data.source.clone()).unwrap_or_default(),
            shared_system_memory: batch.get(self.memory).ok().map(memory),
            dedicated_memory_total_bytes: None,
            dedicated_memory_used_bytes: None,
            memory_scope:
                "shared_system_ram; GPU-only allocation is not available from this collector".into(),
            source_timestamp: gpu.map(|data| protocol::timestamp(data.observed_at_ns)),
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
    fn combines_collectors_and_reuses_memory_without_stale_data() {
        let cpu = Key::new("cpu");
        let mem = Key::new("memory");
        let gpu = Key::new("gpu");
        let mut system = SystemAssembler::new(cpu, mem, identity());
        let mut graphics = GpuAssembler::new(gpu, mem, identity());
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
        let b =
            pb::GpuSample::decode(graphics.assemble(&batch).unwrap()[0].payload.clone()).unwrap();
        assert_eq!(a.cpu.unwrap().usage_percent, Some(37.5));
        assert_eq!(a.memory, b.shared_system_memory);
        assert!(b.dedicated_memory_used_bytes.is_none());
        let empty = pb::SystemSample::decode(
            system.assemble(&Batch::for_test(2)).unwrap()[0]
                .payload
                .clone(),
        )
        .unwrap();
        assert!(empty.cpu.is_none());
        assert!(empty.memory.is_none());
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
    fn never_joins_threads_from_a_reused_pid() {
        use crate::data::{ProcessData, ThreadGroup};
        let proc_key = Key::new("process");
        let thread_key = Key::new("threads");
        let mut assembler = ProcessAssembler::new(proc_key, Some(thread_key), identity());
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
        let message =
            pb::ProcessSample::decode(assembler.assemble(&batch).unwrap()[0].payload.clone())
                .unwrap();
        assert!(message.processes[0].threads_error.is_some());
        assert_eq!(message.processes[0].instance_id, "boot:42:9");
    }
}
