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
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
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
pub struct GpuMetricsData {
    pub utilization_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub utilization_source: String,
    pub memory_source: String,
    pub memory_scope: String,
    pub utilization_timestamp_ns: Option<u64>,
    pub memory_timestamp_ns: Option<u64>,
    pub utilization_error: Option<String>,
    pub memory_error: Option<String>,
}

#[derive(Debug, Default)]
pub struct GpuAccountingData {
    pub system: GpuMetricsData,
    // Identity includes start_ticks, so a reused PID cannot inherit another process's GPU data.
    pub processes: BTreeMap<(u32, u64), GpuMetricsData>,
    pub missing_process: GpuMetricsData,
}
