//! Startup selection belongs to the device module, not app or message assembly.
use super::{
    gpu::JetsonCollector,
    gpu_accounting::{AccountingSources, Capability},
    procfs::Selector,
};
use crate::{
    data::{GpuData, GpuMemoryData, ProcessGpuUtilizationData},
    framework::OperationContext,
};
use std::{path::PathBuf, time::Duration};

pub struct GpuSettings {
    pub sys_root: PathBuf,
    pub load_path: Option<PathBuf>,
    pub tegrastats: PathBuf,
    pub nvmap_clients_path: Option<PathBuf>,
    pub nvidia_smi: PathBuf,
    pub timeout: Duration,
    pub utilization_interval: Duration,
}
pub struct GpuSources {
    pub utilization: Capability<GpuData>,
    pub memory: Capability<GpuMemoryData>,
    pub process_utilization: Capability<ProcessGpuUtilizationData>,
}
impl GpuSources {
    pub fn probe(
        settings: GpuSettings,
        proc_root: PathBuf,
        selector: Option<Selector>,
        context: &OperationContext,
    ) -> Self {
        let jetson = JetsonCollector::probe(
            &settings.sys_root,
            settings.load_path,
            settings.tegrastats,
            settings.timeout,
            settings.utilization_interval,
            context,
        );
        let accounting = AccountingSources::probe(
            settings
                .nvmap_clients_path
                .unwrap_or_else(|| settings.sys_root.join("kernel/debug/nvmap/iovmm/clients")),
            settings.nvidia_smi,
            settings.timeout,
            proc_root,
            selector,
            jetson.is_err(),
            context,
        );
        let utilization = match jetson {
            Ok(source) => {
                Ok(Box::new(source) as Box<dyn crate::framework::Collector<Data = GpuData>>)
            }
            Err(error) => accounting
                .system_utilization
                .map_err(|smi| format!("sysfs/tegrastats: {error:#}; nvidia-smi: {smi}")),
        };
        Self {
            utilization,
            memory: accounting.memory,
            process_utilization: accounting.process_utilization,
        }
    }
}
