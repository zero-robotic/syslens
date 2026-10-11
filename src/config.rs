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
    #[arg(long)]
    pub interval_ms: Option<u64>,
    /// Stop after this many sampling rounds. The first round establishes CPU baselines.
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
    pub processes: ProcessConfig,
    pub gpu: GpuConfig,
    pub recording: RecordingConfig,
    pub foxglove: FoxgloveConfig,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            samples: None,
            proc_root: "/proc".into(),
            processes: ProcessConfig::default(),
            gpu: GpuConfig::default(),
            recording: RecordingConfig::default(),
            foxglove: FoxgloveConfig::default(),
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
}
impl Default for FoxgloveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1".into(),
            port: 8765,
            queue_capacity: 128,
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
        ensure!(self.interval_ms > 0, "interval_ms must be positive");
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
}
