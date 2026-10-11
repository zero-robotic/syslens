use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde::Deserialize;
use std::{fs, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Linux system/process/thread and Jetson telemetry for Foxglove"
)]
pub struct Cli {
    /// Configuration file, relative to the working directory unless an absolute path is given.
    #[arg(short, long, default_value = "config/syslens.toml")]
    pub config: PathBuf,
    /// Live display interval; recording preserves every observation.
    #[arg(long)]
    pub interval_ms: Option<u64>,
    /// Stop after this many interval-ms time windows; not a message count.
    #[arg(long)]
    pub samples: Option<u64>,
    /// Collect these PIDs instead of the configured process selector.
    #[arg(long, value_delimiter = ',')]
    pub pid: Vec<u32>,
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    #[arg(long)]
    pub bind: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub no_gpu: bool,
    #[arg(long)]
    pub no_threads: bool,
    #[arg(long)]
    pub no_live: bool,
    #[arg(long)]
    pub no_recording: bool,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub interval_ms: u64,
    pub samples: Option<u64>,
    pub proc_root: PathBuf,
    pub sampling: SamplingConfig,
    pub runtime: RuntimeConfig,
    pub processes: ProcessConfig,
    pub gpu: GpuConfig,
    pub recording: RecordingConfig,
    pub foxglove: FoxgloveConfig,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            interval_ms: 100,
            samples: None,
            proc_root: "/proc".into(),
            sampling: SamplingConfig::default(),
            runtime: RuntimeConfig::default(),
            processes: ProcessConfig::default(),
            gpu: GpuConfig::default(),
            recording: RecordingConfig::default(),
            foxglove: FoxgloveConfig::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub observation_queue_capacity: usize,
    pub enqueue_timeout_ms: u64,
    pub collection_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
    pub startup_timeout_ms: u64,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            observation_queue_capacity: 1024,
            enqueue_timeout_ms: 1000,
            collection_timeout_ms: 5000,
            shutdown_timeout_ms: 5000,
            startup_timeout_ms: 15000,
        }
    }
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowMode {
    Disable,
    DropNewest,
    DropOldest,
}
impl From<OverflowMode> for crate::framework::Overflow {
    fn from(value: OverflowMode) -> Self {
        match value {
            OverflowMode::Disable => Self::Disable,
            OverflowMode::DropNewest => Self::DropNewest,
            OverflowMode::DropOldest => Self::DropOldest,
        }
    }
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureMode {
    Isolate,
    Stop,
}
impl From<FailureMode> for crate::framework::FailurePolicy {
    fn from(value: FailureMode) -> Self {
        match value {
            FailureMode::Isolate => Self::Isolate,
            FailureMode::Stop => Self::Stop,
        }
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SamplingConfig {
    pub system_cpu_ms: u64,
    pub system_memory_ms: u64,
    pub process_cpu_ms: u64,
    pub process_memory_ms: u64,
    pub thread_cpu_ms: u64,
    pub gpu_utilization_ms: u64,
    pub gpu_memory_ms: u64,
    pub process_gpu_utilization_ms: u64,
    pub stale_after_periods: u32,
}
impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            system_cpu_ms: 100,
            system_memory_ms: 200,
            process_cpu_ms: 100,
            process_memory_ms: 200,
            thread_cpu_ms: 100,
            gpu_utilization_ms: 100,
            gpu_memory_ms: 500,
            process_gpu_utilization_ms: 1000,
            stale_after_periods: 3,
        }
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessConfig {
    pub enabled: bool,
    pub threads: bool,
    pub pids: Vec<u32>,
    pub names: Vec<String>,
}
impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threads: true,
            pids: Vec::new(),
            names: Vec::new(),
        }
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GpuConfig {
    pub enabled: bool,
    pub sys_root: PathBuf,
    pub load_path: Option<PathBuf>,
    pub tegrastats: PathBuf,
    pub nvmap_clients_path: Option<PathBuf>,
    pub nvidia_smi: PathBuf,
    pub timeout_ms: u64,
}
impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sys_root: "/sys".into(),
            load_path: None,
            tegrastats: "tegrastats".into(),
            nvmap_clients_path: None,
            nvidia_smi: "nvidia-smi".into(),
            timeout_ms: 1500,
        }
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    pub enabled: bool,
    pub directory: PathBuf,
    pub queue_capacity: usize,
    pub overflow: OverflowMode,
    pub on_failure: FailureMode,
    pub operation_timeout_ms: u64,
    pub rotate_seconds: u64,
    pub rotate_payload_bytes: u64,
    pub flush_ms: u64,
}
impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "recordings".into(),
            queue_capacity: 128,
            overflow: OverflowMode::Disable,
            on_failure: FailureMode::Isolate,
            operation_timeout_ms: 5000,
            rotate_seconds: 300,
            rotate_payload_bytes: 64 * 1024 * 1024,
            flush_ms: 1000,
        }
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FoxgloveConfig {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
    pub queue_capacity: usize,
    pub overflow: OverflowMode,
    pub on_failure: FailureMode,
    pub operation_timeout_ms: u64,
}
impl Default for FoxgloveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1".into(),
            port: 8765,
            queue_capacity: 128,
            overflow: OverflowMode::DropOldest,
            on_failure: FailureMode::Isolate,
            operation_timeout_ms: 5000,
        }
    }
}

impl Config {
    pub fn load(cli: Cli) -> Result<Self> {
        let path = cli.config;
        let contents = fs::read_to_string(&path).with_context(|| {
            format!(
                "cannot read config {}; use --config to specify a configuration file",
                path.display()
            )
        })?;
        let mut config: Self = toml::from_str(&contents)
            .with_context(|| format!("cannot parse config {}", path.display()))?;
        if let Some(value) = cli.interval_ms {
            config.interval_ms = value;
        }
        if let Some(value) = cli.samples {
            config.samples = Some(value);
        }
        if !cli.pid.is_empty() {
            config.processes.pids = cli.pid;
            config.processes.names.clear();
            config.processes.enabled = true;
        }
        if let Some(value) = cli.output_dir {
            config.recording.directory = value;
        }
        if let Some(value) = cli.bind {
            config.foxglove.bind = value;
        }
        if let Some(value) = cli.port {
            config.foxglove.port = value;
        }
        if cli.no_gpu {
            config.gpu.enabled = false;
        }
        if cli.no_threads {
            config.processes.threads = false;
        }
        if cli.no_live {
            config.foxglove.enabled = false;
        }
        if cli.no_recording {
            config.recording.enabled = false;
        }
        config.validate()?;
        Ok(config)
    }
    fn validate(&mut self) -> Result<()> {
        ensure!(
            (1..=86_400_000).contains(&self.interval_ms),
            "interval_ms must be between 1 ms and 24 hours"
        );
        ensure!(
            self.samples
                .is_none_or(|count| self.interval_ms.checked_mul(count).is_some()),
            "run duration overflow"
        );
        ensure!(
            self.runtime.observation_queue_capacity > 0,
            "observation queue must be positive"
        );
        for timeout in [
            self.runtime.enqueue_timeout_ms,
            self.runtime.collection_timeout_ms,
            self.runtime.shutdown_timeout_ms,
            self.runtime.startup_timeout_ms,
            self.recording.operation_timeout_ms,
            self.foxglove.operation_timeout_ms,
        ] {
            ensure!(
                (1..=86_400_000).contains(&timeout),
                "timeouts must be 1 ms to 24 hours"
            );
        }
        let sampling = &self.sampling;
        for (name, value) in [
            ("system_cpu_ms", sampling.system_cpu_ms),
            ("system_memory_ms", sampling.system_memory_ms),
            ("process_cpu_ms", sampling.process_cpu_ms),
            ("process_memory_ms", sampling.process_memory_ms),
            ("thread_cpu_ms", sampling.thread_cpu_ms),
            ("gpu_utilization_ms", sampling.gpu_utilization_ms),
            ("gpu_memory_ms", sampling.gpu_memory_ms),
            (
                "process_gpu_utilization_ms",
                sampling.process_gpu_utilization_ms,
            ),
        ] {
            ensure!(
                (1..=86_400_000).contains(&value),
                "sampling.{name} must be between 1 ms and 24 hours"
            );
        }
        ensure!(
            sampling.process_gpu_utilization_ms >= 1000,
            "sampling.process_gpu_utilization_ms must be at least 1000 for nvidia-smi pmon"
        );
        ensure!(
            sampling.stale_after_periods > 0,
            "sampling.stale_after_periods must be positive"
        );
        ensure!(self.samples != Some(0), "samples must be positive");
        ensure!(
            self.recording.enabled || self.foxglove.enabled,
            "enable at least one output"
        );
        ensure!(
            self.recording.queue_capacity > 0 && self.foxglove.queue_capacity > 0,
            "queue capacities must be positive"
        );
        ensure!(
            self.recording.rotate_seconds > 0
                && self.recording.rotate_payload_bytes > 0
                && self.recording.flush_ms > 0,
            "recording limits must be positive"
        );
        ensure!(self.gpu.timeout_ms > 0, "GPU timeout must be positive");
        ensure!(
            self.processes.pids.iter().all(|pid| *pid > 0),
            "PIDs must be positive"
        );
        self.processes.pids.sort_unstable();
        self.processes.pids.dedup();
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_settings_and_rejects_typos() {
        assert!(toml::from_str::<Config>("interva_ms = 100").is_err());
        assert!(Config::load(Cli::parse_from(["syslens", "--interval-ms", "0"])).is_err());
        assert!(Config::load(Cli::parse_from(["syslens", "--no-live", "--no-recording"])).is_err());
        let config =
            Config::load(Cli::parse_from(["syslens", "--pid", "1,1,2", "--no-gpu"])).unwrap();
        assert_eq!(config.processes.pids, vec![1, 2]);
        assert!(!config.gpu.enabled);
    }

    #[test]
    fn independent_periods_load_and_invalid_periods_are_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "interval_ms = 50\n[sampling]\nsystem_cpu_ms = 70\nsystem_memory_ms = 350\nprocess_cpu_ms = 90\nprocess_memory_ms = 270\nthread_cpu_ms = 120\ngpu_utilization_ms = 150\ngpu_memory_ms = 700\nprocess_gpu_utilization_ms = 2000\nstale_after_periods = 4\n").unwrap();
        let config = Config::load(Cli::parse_from([
            "syslens",
            "--config",
            file.path().to_str().unwrap(),
            "--interval-ms",
            "25",
        ]))
        .unwrap();
        assert_eq!(config.interval_ms, 25);
        assert_eq!(config.sampling.system_cpu_ms, 70);
        assert_eq!(config.sampling.system_memory_ms, 350);
        assert_eq!(config.sampling.process_cpu_ms, 90);
        assert_eq!(config.sampling.process_memory_ms, 270);
        assert_eq!(config.sampling.thread_cpu_ms, 120);
        assert_eq!(config.sampling.gpu_utilization_ms, 150);
        assert_eq!(config.sampling.gpu_memory_ms, 700);
        assert_eq!(config.sampling.process_gpu_utilization_ms, 2000);
        assert_eq!(config.sampling.stale_after_periods, 4);
        for settings in [
            "system_cpu_ms = 0",
            "thread_cpu_ms = 0",
            "gpu_memory_ms = 0",
            "process_gpu_utilization_ms = 999",
            "stale_after_periods = 0",
            "system_memory_ms = 86400001",
        ] {
            let mut config: Config = toml::from_str(&format!("[sampling]\n{settings}")).unwrap();
            assert!(
                config.validate().is_err(),
                "accepted invalid setting: {settings}"
            );
        }
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    #[test]
    fn runtime_and_output_policies_are_configurable_and_invalid_limits_rejected() {
        let mut config: Config = toml::from_str("[runtime]\nobservation_queue_capacity = 17\nenqueue_timeout_ms = 200\nshutdown_timeout_ms = 3000\n[recording]\noverflow = 'drop_newest'\non_failure = 'stop'\noperation_timeout_ms = 600\n").unwrap();
        config.validate().unwrap();
        assert_eq!(config.runtime.observation_queue_capacity, 17);
        assert_eq!(config.runtime.enqueue_timeout_ms, 200);
        assert!(matches!(
            config.recording.overflow,
            OverflowMode::DropNewest
        ));
        assert!(matches!(config.recording.on_failure, FailureMode::Stop));
        for text in [
            "[runtime]\nobservation_queue_capacity = 0",
            "[runtime]\ncollection_timeout_ms = 0",
            "[runtime]\nshutdown_timeout_ms = 0",
            "[foxglove]\noperation_timeout_ms = 0",
        ] {
            let mut config: Config = toml::from_str(text).unwrap();
            assert!(config.validate().is_err());
        }
        assert!(toml::from_str::<Config>("[recording]\noverflow = 'block'").is_err());
        assert!(toml::from_str::<Config>("[recording]\non_failure = 'ignore'").is_err());
    }
}
