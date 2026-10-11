use crate::{
    data::{
        CpuData, GpuData, GpuMemory, GpuMemoryData, GpuUtilization, MemoryData,
        ProcessGpuUtilizationData, ProcessMemory, Processes, Threads,
    },
    framework::{Assembler, Batch, Dependency, Key, Sample, StreamDescriptor},
    protocol::{self, pb},
};
use anyhow::Result;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub struct Identity {
    pub host: String,
    pub boot_id: String,
}
impl Identity {
    fn header(&self, batch: &Batch, dependencies: &[Dependency]) -> pb::Header {
        pb::Header {
            timestamp: Some(protocol::timestamp(batch.captured_at_ns)),
            batch_id: batch.id,
            host: self.host.clone(),
            boot_id: self.boot_id.clone(),
            trigger_input: batch.trigger.into(),
            inputs: dependencies
                .iter()
                .map(|input| {
                    let status = batch.status(input.name);
                    #[allow(deprecated)]
                    pb::InputStatus {
                        collector: input.name.into(),
                        timestamp: status.captured_at_ns.map(protocol::timestamp),
                        error: status.error,
                        interval_ms: status.interval_ms,
                        actual_interval_ms: status.actual_interval_ms,
                        collection_duration_ms: status.collection_duration_ms,
                        age_ms: status.age_ms,
                        stale: status.stale,
                        sample_sequence: status.sample_sequence,
                        missed_periods: status.missed_periods,
                        skipped_samples: 0,
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
fn gpu_metrics(memory: GpuMemory, utilization: GpuUtilization) -> pb::GpuMetrics {
    pb::GpuMetrics {
        utilization_percent: utilization.percent,
        memory_used_bytes: memory.used_bytes,
        memory_total_bytes: memory.total_bytes,
        utilization_source: utilization.source,
        memory_source: memory.source,
        memory_scope: memory.scope,
        utilization_timestamp: utilization.observed_at_ns.map(protocol::timestamp),
        memory_timestamp: memory.observed_at_ns.map(protocol::timestamp),
        utilization_error: utilization.error,
        memory_error: memory.error,
    }
}
#[derive(Clone, Copy)]
pub struct GpuInputs {
    pub utilization: Key<GpuData>,
    pub memory: Key<GpuMemoryData>,
    pub process_utilization: Key<ProcessGpuUtilizationData>,
}
impl GpuInputs {
    fn system(&self, batch: &Batch) -> Result<pb::GpuMetrics> {
        let memory = batch
            .read(self.memory)?
            .map(|data| data.system.clone())
            .unwrap_or_else(|| GpuMemory {
                error: batch.status(self.memory.name).error,
                ..Default::default()
            });
        let utilization = batch
            .read(self.utilization)?
            .map(|data| GpuUtilization {
                percent: Some(data.utilization_percent),
                source: data.source.clone(),
                observed_at_ns: Some(data.observed_at_ns),
                error: None,
            })
            .unwrap_or_else(|| GpuUtilization {
                error: batch.status(self.utilization.name).error,
                ..Default::default()
            });
        Ok(gpu_metrics(memory, utilization))
    }
    fn process(&self, batch: &Batch, id: (u32, u64)) -> Result<pb::GpuMetrics> {
        let memory = batch
            .read(self.memory)?
            .map(|data| {
                data.processes
                    .get(&id)
                    .map_or(&data.missing_process, |row| &row.value)
                    .clone()
            })
            .unwrap_or_else(|| GpuMemory {
                error: batch.status(self.memory.name).error,
                ..Default::default()
            });
        let utilization = batch
            .read(self.process_utilization)?
            .map(|data| {
                data.processes
                    .get(&id)
                    .map_or(&data.missing_process, |row| &row.value)
                    .clone()
            })
            .unwrap_or_else(|| GpuUtilization {
                error: batch.status(self.process_utilization.name).error,
                ..Default::default()
            });
        Ok(gpu_metrics(memory, utilization))
    }
}
pub struct SystemAssembler {
    cpu: Key<CpuData>,
    memory: Key<MemoryData>,
    gpu: Option<GpuInputs>,
    dependencies: Vec<Dependency>,
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
        let mut dependencies = vec![cpu.dependency(), memory.dependency()];
        if let Some(gpu) = gpu {
            dependencies.extend([gpu.utilization.dependency(), gpu.memory.dependency()]);
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
    fn dependencies(&self) -> &[Dependency] {
        &self.dependencies
    }
    fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>> {
        let message = pb::SystemSample {
            header: Some(self.identity.header(batch, &self.dependencies)),
            cpu: batch.read(self.cpu)?.map(|data| pb::Cpu {
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
            memory: batch.read(self.memory)?.map(memory),
            gpu: self.gpu.map(|gpu| gpu.system(batch)).transpose()?,
        };
        Ok(vec![protocol::encode(
            &message,
            self.stream.clone(),
            batch.captured_at_ns,
        )])
    }
}
pub struct ProcessAssembler {
    cpu: Key<Processes>,
    memory: Key<ProcessMemory>,
    threads: Option<Key<Threads>>,
    gpu: Option<GpuInputs>,
    dependencies: Vec<Dependency>,
    identity: Identity,
    stream: Arc<StreamDescriptor>,
}
impl ProcessAssembler {
    pub fn new(
        cpu: Key<Processes>,
        memory: Key<ProcessMemory>,
        threads: Option<Key<Threads>>,
        gpu: Option<GpuInputs>,
        identity: Identity,
    ) -> Self {
        let mut dependencies = vec![cpu.dependency(), memory.dependency()];
        if let Some(key) = threads {
            dependencies.push(key.dependency());
        }
        if let Some(gpu) = gpu {
            dependencies.extend([
                gpu.memory.dependency(),
                gpu.process_utilization.dependency(),
            ]);
        }
        Self {
            cpu,
            memory,
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
    fn dependencies(&self) -> &[Dependency] {
        &self.dependencies
    }
    fn assemble(&mut self, batch: &Batch) -> Result<Vec<Sample>> {
        let cpu = batch.read(self.cpu)?;
        let memory = batch.read(self.memory)?;
        let threads = self
            .threads
            .map(|key| batch.read(key))
            .transpose()?
            .flatten();
        let cpu_rows: BTreeMap<_, _> = cpu
            .into_iter()
            .flat_map(|data| &data.items)
            .map(|row| ((row.pid, row.start_ticks), row))
            .collect();
        let groups: BTreeMap<_, _> = threads
            .into_iter()
            .flat_map(|data| &data.groups)
            .map(|row| ((row.pid, row.process_start_ticks), row))
            .collect();
        // Each source owns the identities it observed. No metric is the entity roster.
        let mut identities = BTreeMap::<(u32, u64), &str>::new();
        for (&id, row) in &cpu_rows {
            identities.insert(id, &row.name);
        }
        if let Some(memory) = memory {
            for (&id, row) in &memory.items {
                identities.entry(id).or_insert(&row.name);
            }
        }
        for (&id, row) in &groups {
            identities.entry(id).or_insert(&row.process_name);
        }
        if let Some(gpu) = self.gpu {
            if let Some(data) = batch.read(gpu.memory)? {
                for (&id, row) in &data.processes {
                    identities.entry(id).or_insert(&row.name);
                }
            }
            if let Some(data) = batch.read(gpu.process_utilization)? {
                for (&id, row) in &data.processes {
                    identities.entry(id).or_insert(&row.name);
                }
            }
        }
        let mut processes = Vec::new();
        for (id @ (pid, start), name) in identities {
            let cpu = cpu_rows.get(&id);
            let mem = memory.and_then(|data| data.items.get(&id));
            let group = groups.get(&id);
            let instance_id = self.identity.process_id(pid, start);
            processes.push(pb::Process {
                pid,
                start_ticks: start,
                instance_id: instance_id.clone(),
                name: name.into(),
                cpu_percent: cpu.and_then(|row| row.cpu_percent),
                cpu_time_ticks: cpu.map(|row| row.cpu_ticks),
                rss_bytes: mem.map(|row| row.rss_bytes),
                virtual_bytes: mem.map(|row| row.virtual_bytes),
                threads: group
                    .into_iter()
                    .flat_map(|group| &group.items)
                    .map(|thread| pb::Thread {
                        tid: thread.tid,
                        start_ticks: thread.start_ticks,
                        instance_id: format!("{instance_id}:{}:{}", thread.tid, thread.start_ticks),
                        name: thread.name.clone(),
                        cpu_percent: thread.cpu_percent,
                        cpu_time_ticks: thread.cpu_ticks,
                    })
                    .collect(),
                threads_error: self.threads.and_then(|key| {
                    group.map_or_else(
                        || {
                            batch.status(key.name).error.or_else(|| {
                                Some(
                                    "no matching thread observation for this process instance"
                                        .into(),
                                )
                            })
                        },
                        |group| group.error.clone(),
                    )
                }),
                gpu: self.gpu.map(|gpu| gpu.process(batch, id)).transpose()?,
            });
        }
        let message = pb::ProcessSample {
            header: Some(self.identity.header(batch, &self.dependencies)),
            processes,
            skipped_processes: cpu.map_or(0, |data| data.skipped),
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
    use crate::data::{Named, ProcessData, ProcessMemoryData, ThreadGroup};
    use prost::Message;
    fn identity() -> Identity {
        Identity {
            host: "test".into(),
            boot_id: "boot".into(),
        }
    }
    #[test]
    fn merges_independent_entity_sets_without_cpu_or_crossing_pid_instances() {
        let cpu = Key::new("cpu");
        let mem = Key::new("memory");
        let thread = Key::new("thread");
        let gpu = GpuInputs {
            utilization: Key::new("gpu"),
            memory: Key::new("gpu_mem"),
            process_utilization: Key::new("gpu_process"),
        };
        let mut assembler = ProcessAssembler::new(cpu, mem, Some(thread), Some(gpu), identity());
        let mut batch = Batch::for_test(0);
        batch.unavailable(cpu, "read failed");
        batch.insert(
            mem,
            ProcessMemory {
                items: [(
                    (42, 9),
                    ProcessMemoryData {
                        name: "memory-only".into(),
                        rss_bytes: 2048,
                        virtual_bytes: 4096,
                    },
                )]
                .into(),
            },
        );
        batch.insert(
            thread,
            Threads {
                groups: vec![ThreadGroup {
                    process_name: "thread-only".into(),
                    pid: 43,
                    process_start_ticks: 10,
                    items: vec![],
                    error: None,
                }],
            },
        );
        batch.insert(
            gpu.memory,
            GpuMemoryData {
                processes: [(
                    (44, 11),
                    Named {
                        name: "gpu-only".into(),
                        value: GpuMemory {
                            used_bytes: Some(8192),
                            ..Default::default()
                        },
                    },
                )]
                .into(),
                ..Default::default()
            },
        );
        batch.unavailable(gpu.process_utilization, "unsupported");
        let message =
            pb::ProcessSample::decode(assembler.assemble(&batch).unwrap()[0].payload.clone())
                .unwrap();
        assert_eq!(message.processes.len(), 3);
        assert_eq!(message.processes[0].rss_bytes, Some(2048));
        assert_eq!(message.processes[0].name, "memory-only");
        assert!(
            message
                .processes
                .iter()
                .all(|row| row.cpu_percent.is_none() && row.cpu_time_ticks.is_none())
        );
        assert_eq!(
            message.processes[2].gpu.as_ref().unwrap().memory_used_bytes,
            Some(8192)
        );
        batch.insert(
            cpu,
            Processes {
                items: vec![ProcessData {
                    pid: 42,
                    start_ticks: 100,
                    name: "reused".into(),
                    cpu_percent: Some(50.0),
                    cpu_ticks: 1,
                }],
                skipped: 0,
            },
        );
        let message =
            pb::ProcessSample::decode(assembler.assemble(&batch).unwrap()[0].payload.clone())
                .unwrap();
        let reused = message
            .processes
            .iter()
            .find(|row| row.start_ticks == 100)
            .unwrap();
        assert!(reused.rss_bytes.is_none());
        assert!(reused.threads.is_empty());
        assert_eq!(reused.instance_id, "boot:42:100");
    }
    #[test]
    fn system_assembly_preserves_partial_metrics_and_propagates_wiring_errors() {
        let cpu = Key::new("cpu");
        let memory_key = Key::new("memory");
        let mut assembler = SystemAssembler::new(cpu, memory_key, None, identity());
        let mut batch = Batch::for_test(0);
        batch.unavailable(cpu, "not ready");
        batch.insert(
            memory_key,
            MemoryData {
                total_bytes: 4096,
                available_bytes: 1024,
                ..Default::default()
            },
        );
        let message =
            pb::SystemSample::decode(assembler.assemble(&batch).unwrap()[0].payload.clone())
                .unwrap();
        assert!(message.cpu.is_none());
        assert_eq!(message.memory.unwrap().used_bytes, 3072);
        batch.insert(Key::<u64>::new("cpu"), 7);
        assert!(
            assembler
                .assemble(&batch)
                .unwrap_err()
                .to_string()
                .contains("type mismatch")
        );
    }
}
