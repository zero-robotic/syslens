use crate::{
    assemblers::{GpuInputs, Identity, ProcessAssembler, SystemAssembler},
    collectors::{
        cpu::CpuCollector,
        gpu_sources::{GpuSettings, GpuSources},
        memory::MemoryCollector,
        process::{ProcessCollector, ProcessMemoryCollector, Selector},
        threads::ThreadCollector,
    },
    config::{Cli, Config},
    data::{
        CpuData, GpuData, GpuMemoryData, MemoryData, ProcessGpuUtilizationData, ProcessMemory,
        Processes, Threads,
    },
    framework::{
        Cancellation, Delivery, Dispatcher, Key, OutputOptions, Runtime, RuntimeOptions, Schedule,
        run_bounded,
    },
    outputs::{foxglove::FoxgloveOutput, mcap::McapOutput},
};
use anyhow::{Result, ensure};
use clap::Parser;
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    ensure!(
        cfg!(target_os = "linux"),
        "syslens only supports Linux; build and run on Linux or Jetson"
    );
    let config = Config::load(cli)?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = stop.clone();
    ctrlc::set_handler(move || signal_stop.store(true, Ordering::Relaxed))?;
    run_config(config, stop)
}

pub fn run_config(config: Config, stop: Arc<AtomicBool>) -> Result<()> {
    let identity = Identity {
        host: fs::read_to_string(config.proc_root.join("sys/kernel/hostname"))?
            .trim()
            .into(),
        boot_id: fs::read_to_string(config.proc_root.join("sys/kernel/random/boot_id"))?
            .trim()
            .into(),
    };
    let mut dispatcher = Dispatcher::default();
    if config.foxglove.enabled {
        dispatcher.add(
            "foxglove",
            FoxgloveOutput::new(&config.foxglove.bind, config.foxglove.port)?,
            OutputOptions {
                capacity: config.foxglove.queue_capacity,
                delivery: Delivery::Latest {
                    interval: Duration::from_millis(config.interval_ms),
                },
                overflow: config.foxglove.overflow.into(),
                failure: config.foxglove.on_failure.into(),
                operation_timeout: Duration::from_millis(config.foxglove.operation_timeout_ms),
            },
        )?;
    }
    if config.recording.enabled {
        dispatcher.add(
            "mcap",
            McapOutput::new(
                config.recording.directory,
                Duration::from_secs(config.recording.rotate_seconds),
                Duration::from_millis(config.recording.flush_ms),
                config.recording.rotate_payload_bytes,
            )?,
            OutputOptions {
                capacity: config.recording.queue_capacity,
                delivery: Delivery::All,
                overflow: config.recording.overflow.into(),
                failure: config.recording.on_failure.into(),
                operation_timeout: Duration::from_millis(config.recording.operation_timeout_ms),
            },
        )?;
    }
    let mut runtime = Runtime::new(
        dispatcher,
        RuntimeOptions {
            ingress_capacity: config.runtime.observation_queue_capacity,
            ingress_timeout: Duration::from_millis(config.runtime.enqueue_timeout_ms),
            shutdown_timeout: Duration::from_millis(config.runtime.shutdown_timeout_ms),
        },
    )?;
    let sampling = &config.sampling;
    let schedule = |ms| {
        Schedule::new(
            Duration::from_millis(ms),
            sampling.stale_after_periods,
            Duration::from_millis(config.runtime.collection_timeout_ms),
        )
    };
    let cpu = Key::<CpuData>::new("cpu");
    let memory = Key::<MemoryData>::new("memory");
    runtime.add_collector(
        cpu,
        CpuCollector::new(&config.proc_root),
        schedule(sampling.system_cpu_ms)?,
    )?;
    runtime.add_collector(
        memory,
        MemoryCollector::new(&config.proc_root),
        schedule(sampling.system_memory_ms)?,
    )?;
    let selector = Selector {
        pids: config.processes.pids,
        names: config.processes.names,
    };
    let gpu = if config.gpu.enabled {
        let settings = GpuSettings {
            sys_root: config.gpu.sys_root,
            load_path: config.gpu.load_path,
            tegrastats: config.gpu.tegrastats,
            nvmap_clients_path: config.gpu.nvmap_clients_path,
            nvidia_smi: config.gpu.nvidia_smi,
            timeout: Duration::from_millis(config.gpu.timeout_ms),
            utilization_interval: Duration::from_millis(sampling.gpu_utilization_ms),
        };
        let proc_root = config.proc_root.clone();
        let process_selector = config.processes.enabled.then(|| selector.clone());
        let sources = run_bounded(
            "gpu-probe",
            Duration::from_millis(config.runtime.startup_timeout_ms),
            Duration::from_millis(config.runtime.shutdown_timeout_ms),
            Cancellation::from_flag(stop.clone()),
            move |context| {
                Ok(GpuSources::probe(
                    settings,
                    proc_root,
                    process_selector,
                    context,
                ))
            },
        )?;
        let inputs = GpuInputs {
            utilization: Key::<GpuData>::new("gpu_utilization"),
            memory: Key::<GpuMemoryData>::new("gpu_memory"),
            process_utilization: Key::<ProcessGpuUtilizationData>::new("process_gpu_utilization"),
        };
        runtime.add_capability(
            inputs.utilization,
            sources.utilization,
            schedule(sampling.gpu_utilization_ms)?,
        )?;
        runtime.add_capability(
            inputs.memory,
            sources.memory,
            schedule(sampling.gpu_memory_ms)?,
        )?;
        if config.processes.enabled {
            runtime.add_capability(
                inputs.process_utilization,
                sources.process_utilization,
                schedule(sampling.process_gpu_utilization_ms)?,
            )?;
        }
        Some(inputs)
    } else {
        None
    };
    runtime.add_assembler(SystemAssembler::new(cpu, memory, gpu, identity.clone()))?;
    if config.processes.enabled {
        let key = Key::<Processes>::new("process_cpu");
        runtime.add_collector(
            key,
            ProcessCollector::new(config.proc_root.clone(), selector.clone())?,
            schedule(sampling.process_cpu_ms)?,
        )?;
        let process_memory = Key::<ProcessMemory>::new("process_memory");
        runtime.add_collector(
            process_memory,
            ProcessMemoryCollector::new(config.proc_root.clone(), selector.clone())?,
            schedule(sampling.process_memory_ms)?,
        )?;
        let thread_key = if config.processes.threads {
            let key = Key::<Threads>::new("thread_cpu");
            runtime.add_collector(
                key,
                ThreadCollector::new(config.proc_root.clone(), selector)?,
                schedule(sampling.thread_cpu_ms)?,
            )?;
            Some(key)
        } else {
            None
        };
        runtime.add_assembler(ProcessAssembler::new(
            key,
            process_memory,
            thread_key,
            gpu,
            identity,
        ))?;
    }
    let duration = config
        .samples
        .map(|count| {
            config
                .interval_ms
                .checked_mul(count)
                .map(Duration::from_millis)
                .ok_or_else(|| anyhow::anyhow!("run duration overflow"))
        })
        .transpose()?;
    runtime.run(duration, stop)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::protocol::pb;
    use prost::Message;
    fn messages(directory: &std::path::Path) -> (Vec<pb::SystemSample>, Vec<pb::ProcessSample>) {
        let mut systems = Vec::new();
        let mut processes = Vec::new();
        for file in fs::read_dir(directory).unwrap() {
            let bytes = fs::read(file.unwrap().path()).unwrap();
            assert!(mcap::Summary::read(&bytes).unwrap().is_some());
            for message in mcap::MessageStream::new(&bytes).unwrap() {
                let message = message.unwrap();
                match message.channel.topic.as_str() {
                    "/syslens/system" => {
                        let sample = pb::SystemSample::decode(message.data.as_ref()).unwrap();
                        let timestamp = sample.header.as_ref().unwrap().timestamp.as_ref().unwrap();
                        assert_eq!(
                            message.log_time,
                            timestamp.seconds as u64 * 1_000_000_000 + timestamp.nanos as u64
                        );
                        systems.push(sample);
                    }
                    "/syslens/processes" => {
                        processes.push(pb::ProcessSample::decode(message.data.as_ref()).unwrap())
                    }
                    other => panic!("unexpected topic {other}"),
                }
            }
        }
        (systems, processes)
    }
    #[test]
    fn unavailable_gpu_is_probed_once_and_typed_status_remains_visible() {
        use std::os::unix::fs::PermissionsExt;
        for enabled in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let executable = dir.path().join("tegrastats");
            fs::write(
                &executable,
                "#!/bin/sh\necho started >> \"$0.starts\"\necho invalid\n",
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let mut config = Config {
                samples: Some(3),
                interval_ms: 50,
                ..Default::default()
            };
            config.foxglove.enabled = false;
            config.recording.directory = dir.path().join("recordings");
            config.processes.pids = vec![std::process::id()];
            config.gpu.enabled = enabled;
            config.gpu.sys_root = dir.path().join("missing-sysfs");
            config.gpu.tegrastats = executable;
            config.gpu.nvidia_smi = dir.path().join("missing-smi");
            config.gpu.timeout_ms = 100;
            run_config(config, Arc::new(AtomicBool::new(false))).unwrap();
            if enabled {
                assert_eq!(
                    fs::read_to_string(dir.path().join("tegrastats.starts")).unwrap(),
                    "started\n"
                );
            } else {
                assert!(!dir.path().join("tegrastats.starts").exists());
            }
            let (systems, processes) = messages(&dir.path().join("recordings"));
            assert!(!systems.is_empty() && !processes.is_empty());
            for system in systems {
                assert_eq!(system.gpu.is_some(), enabled);
                if enabled {
                    assert!(system.gpu.unwrap().utilization_error.is_some());
                    let header = system.header.unwrap();
                    assert!(
                        header
                            .inputs
                            .iter()
                            .any(|input| input.collector == "gpu_utilization"
                                && input.error.is_some())
                    );
                }
            }
        }
    }
    #[test]
    fn real_linux_observations_keep_sequences_identity_and_gpu_data_in_finalized_mcap() {
        let dir = tempfile::tempdir().unwrap();
        let load = dir.path().join("load");
        let clients = dir.path().join("clients");
        fs::write(&load, "375").unwrap();
        fs::write(
            &clients,
            format!(
                "CLIENT PROCESS PID SIZE\nuser syslens {} 16K\ntotal 32K\n",
                std::process::id()
            ),
        )
        .unwrap();
        let mut config = Config {
            samples: Some(4),
            interval_ms: 100,
            ..Default::default()
        };
        config.foxglove.enabled = false;
        config.recording.directory = dir.path().join("recordings");
        config.gpu.load_path = Some(load);
        config.gpu.nvmap_clients_path = Some(clients);
        config.gpu.nvidia_smi = dir.path().join("missing-smi");
        config.processes.pids = vec![std::process::id()];
        config.sampling.system_cpu_ms = 20;
        config.sampling.process_cpu_ms = 30;
        config.sampling.process_memory_ms = 70;
        config.sampling.thread_cpu_ms = 40;
        config.sampling.gpu_memory_ms = 1000;
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let busy = std::thread::spawn(move || {
            while !signal.load(Ordering::Relaxed) {
                std::hint::black_box((0..1000u64).sum::<u64>());
            }
        });
        let result = run_config(config, Arc::new(AtomicBool::new(false)));
        stop.store(true, Ordering::Relaxed);
        busy.join().unwrap();
        result.unwrap();
        let (systems, processes) = messages(&dir.path().join("recordings"));
        let cpu_sequences: Vec<_> = systems
            .iter()
            .filter(|sample| sample.header.as_ref().unwrap().trigger_input == "cpu")
            .map(|sample| {
                sample
                    .header
                    .as_ref()
                    .unwrap()
                    .inputs
                    .iter()
                    .find(|input| input.collector == "cpu")
                    .unwrap()
                    .sample_sequence
            })
            .collect();
        assert!(
            cpu_sequences.len() > 4,
            "recording incorrectly limited by live interval"
        );
        assert_eq!(
            cpu_sequences,
            (1..=cpu_sequences.len() as u64).collect::<Vec<_>>()
        );
        assert!(systems.iter().any(|s| {
            s.cpu
                .as_ref()
                .is_some_and(|cpu| cpu.usage_percent.is_some())
        }));
        assert!(systems.iter().any(|s| {
            s.gpu.as_ref().is_some_and(|gpu| {
                gpu.utilization_percent == Some(37.5)
                    && gpu.memory_used_bytes == Some(32 * 1024)
                    && gpu.memory_total_bytes.is_none()
            })
        }));
        assert!(
            processes
                .iter()
                .flat_map(|sample| &sample.processes)
                .any(|p| p.cpu_percent.unwrap_or_default() > 0.0)
        );
        assert!(
            processes
                .iter()
                .flat_map(|sample| &sample.processes)
                .any(|p| p.rss_bytes.unwrap_or_default() > 0
                    && !p.threads.is_empty()
                    && p.gpu
                        .as_ref()
                        .is_some_and(|gpu| gpu.memory_used_bytes == Some(16 * 1024)))
        );
        let expected_command_line = std::env::args().collect::<Vec<_>>().join(" ");
        for p in processes.iter().flat_map(|sample| &sample.processes) {
            assert_eq!(p.pid, std::process::id());
            if p.rss_bytes.is_some() {
                assert_eq!(
                    p.command_line.as_deref(),
                    Some(expected_command_line.as_str())
                );
                assert!(p.command_line_error.is_none());
            }
            assert!(
                p.instance_id
                    .ends_with(&format!(":{}:{}", p.pid, p.start_ticks))
            );
        }
    }
}
