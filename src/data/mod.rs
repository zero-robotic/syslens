use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct CpuData {
    pub usage_percent: Option<f64>,
    pub cores: BTreeMap<u32, Option<f64>>,
}
#[derive(Debug, Default, Clone)]
pub struct MemoryData {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
}
#[derive(Debug)]
pub struct ProcessData {
    pub pid: u32,
    pub start_ticks: u64,
    pub name: String,
    pub cpu_percent: Option<f64>,
    pub cpu_ticks: u64,
}
#[derive(Debug)]
pub struct ProcessMemoryData {
    pub name: String,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
}
#[derive(Debug, Default)]
pub struct ProcessMemory {
    pub items: BTreeMap<(u32, u64), ProcessMemoryData>,
}
#[derive(Debug, Default)]
pub struct Processes {
    pub items: Vec<ProcessData>,
    pub skipped: u32,
}

#[derive(Debug)]
pub struct ThreadData {
    pub tid: u32,
    pub start_ticks: u64,
    pub name: String,
    pub cpu_percent: Option<f64>,
    pub cpu_ticks: u64,
}
#[derive(Debug)]
pub struct ThreadGroup {
    pub process_name: String,
    pub pid: u32,
    pub process_start_ticks: u64,
    pub items: Vec<ThreadData>,
    pub error: Option<String>,
}
#[derive(Debug, Default)]
pub struct Threads {
    pub groups: Vec<ThreadGroup>,
}

#[derive(Debug)]
pub struct GpuData {
    pub utilization_percent: f64,
    pub source: String,
    pub observed_at_ns: u64,
}

#[derive(Debug, Default, Clone)]
pub struct GpuMemory {
    pub used_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub source: String,
    pub scope: String,
    pub observed_at_ns: Option<u64>,
    pub error: Option<String>,
}
#[derive(Debug, Default, Clone)]
pub struct GpuUtilization {
    pub percent: Option<f64>,
    pub source: String,
    pub observed_at_ns: Option<u64>,
    pub error: Option<String>,
}
#[derive(Debug, Clone)]
pub struct Named<T> {
    pub name: String,
    pub value: T,
}
#[derive(Debug, Default)]
pub struct GpuMemoryData {
    pub system: GpuMemory,
    pub processes: BTreeMap<(u32, u64), Named<GpuMemory>>,
    pub missing_process: GpuMemory,
}
#[derive(Debug, Default)]
pub struct ProcessGpuUtilizationData {
    pub processes: BTreeMap<(u32, u64), Named<GpuUtilization>>,
    pub missing_process: GpuUtilization,
}
