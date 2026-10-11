use crate::{
    assemblers::{GpuAssembler, Identity, ProcessAssembler, SystemAssembler},
    collectors::{
        cpu::CpuCollector,
        gpu::JetsonCollector,
        memory::MemoryCollector,
        process::{ProcessCollector, Selector},
        threads::ThreadCollector,
    },
    config::{Cli, Config},
    data::{CpuData, GpuData, MemoryData, Processes, Threads},
    framework::{Dispatcher, Key, Overflow, Runtime},
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
            config.foxglove.queue_capacity,
            Overflow::DropNewest,
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
            config.recording.queue_capacity,
            Overflow::Block,
        )?;
    }
    let mut runtime = Runtime::new(dispatcher);
    let cpu = Key::<CpuData>::new("cpu");
    let memory = Key::<MemoryData>::new("memory");
    runtime.add_collector(cpu, CpuCollector::new(&config.proc_root))?;
    runtime.add_collector(memory, MemoryCollector::new(&config.proc_root))?;
    runtime.add_assembler(SystemAssembler::new(cpu, memory, identity.clone()))?;
    if config.processes.enabled {
        let key = Key::<Processes>::new("processes");
        let selector = Selector {
            pids: config.processes.pids,
            names: config.processes.names,
        };
        runtime.add_collector(
            key,
            ProcessCollector::new(config.proc_root.clone(), selector.clone())?,
        )?;
        let thread_key = if config.processes.threads {
            let key = Key::<Threads>::new("threads");
            runtime.add_collector(
                key,
                ThreadCollector::new(config.proc_root.clone(), selector)?,
            )?;
            Some(key)
        } else {
            None
        };
        runtime.add_assembler(ProcessAssembler::new(key, thread_key, identity.clone()))?;
    }
    if config.gpu.enabled {
        match JetsonCollector::probe(
            &config.gpu.sys_root,
            config.gpu.load_path,
            config.gpu.tegrastats,
            Duration::from_millis(config.gpu.timeout_ms),
        ) {
            Ok(collector) => {
                let key = Key::<GpuData>::new("gpu");
                runtime.add_collector(key, collector)?;
                runtime.add_assembler(GpuAssembler::new(key, memory, identity))?;
            }
            Err(error) => {
                eprintln!("Jetson GPU disabled for this run (startup probe failed): {error:#}");
            }
        }
    }
    runtime.run(
        Duration::from_millis(config.interval_ms),
        config.samples,
        stop,
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::protocol::pb;
    use prost::Message;

    #[test]
    fn unavailable_gpu_is_probed_once_and_does_not_block_other_messages() {
        use std::os::unix::fs::PermissionsExt;

        for enabled in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("tegrastats");
            // The executable exists but never reports valid GPU data.
            fs::write(&executable, "#!/bin/sh\nprintf 'started\\n' >> \"$0.starts\"\nwhile :; do\n  printf '%s\\n' 'GR3D_FREQ @[1098,1098]'\n  sleep 0.02\ndone\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let mut config = Config {
                samples: Some(3),
                interval_ms: 10,
                ..Default::default()
            };
            config.foxglove.enabled = false;
            config.recording.directory = directory.path().join("recordings");
            config.processes.pids = vec![std::process::id()];
            config.gpu.enabled = enabled;
            config.gpu.sys_root = directory.path().join("no-sysfs");
            config.gpu.tegrastats = executable;
            config.gpu.timeout_ms = 1000;

            run_config(config, Arc::new(AtomicBool::new(false))).unwrap();
            let starts = directory.path().join("tegrastats.starts");
            if enabled {
                assert_eq!(fs::read_to_string(starts).unwrap(), "started\n");
            } else {
                assert!(!starts.exists(), "disabled GPU must not be probed");
            }
            let file = fs::read_dir(directory.path().join("recordings"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let bytes = fs::read(file).unwrap();
            let mut counts = std::collections::HashMap::new();
            for message in mcap::MessageStream::new(&bytes).unwrap() {
                let message = message.unwrap();
                *counts.entry(message.channel.topic.clone()).or_insert(0) += 1;
            }
            assert_eq!(counts.len(), 2);
            assert_eq!(counts["/syslens/system"], 3);
            assert_eq!(counts["/syslens/processes"], 3);
            assert!(!counts.contains_key("/syslens/gpu"));
        }
    }

    #[test]
    fn real_linux_processes_and_fixture_jetson_reach_finalized_mcap() {
        let directory = tempfile::tempdir().unwrap();
        let load = directory.path().join("gpu-load");
        fs::write(&load, "375\n").unwrap();
        let mut config = Config {
            samples: Some(4),
            interval_ms: 100,
            ..Default::default()
        };
        config.foxglove.enabled = false;
        config.recording.directory = directory.path().join("recordings");
        config.gpu.load_path = Some(load);
        config.processes.pids = vec![std::process::id()];
        let busy_stop = Arc::new(AtomicBool::new(false));
        let busy_signal = busy_stop.clone();
        let busy = std::thread::spawn(move || {
            while !busy_signal.load(Ordering::Relaxed) {
                std::hint::black_box((0..1000u64).sum::<u64>());
                std::hint::spin_loop();
            }
        });
        let result = run_config(config, Arc::new(AtomicBool::new(false)));
        busy_stop.store(true, Ordering::Relaxed);
        busy.join().unwrap();
        result.unwrap();
        let file = fs::read_dir(directory.path().join("recordings"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let bytes = fs::read(file).unwrap();
        let summary = mcap::Summary::read(&bytes).unwrap().unwrap();
        assert_eq!(summary.stats.unwrap().message_count, 12);
        let mut systems = Vec::new();
        let mut processes = Vec::new();
        let mut gpus = Vec::new();
        for message in mcap::MessageStream::new(&bytes).unwrap() {
            let message = message.unwrap();
            match message.channel.topic.as_str() {
                "/syslens/system" => {
                    systems.push(pb::SystemSample::decode(message.data.as_ref()).unwrap())
                }
                "/syslens/processes" => {
                    processes.push(pb::ProcessSample::decode(message.data.as_ref()).unwrap())
                }
                "/syslens/gpu" => gpus.push(pb::GpuSample::decode(message.data.as_ref()).unwrap()),
                topic => panic!("unexpected topic {topic}"),
            }
        }
        assert_eq!((systems.len(), processes.len(), gpus.len()), (4, 4, 4));
        assert!(systems[0].cpu.as_ref().unwrap().usage_percent.is_none());
        assert!(
            systems[1..]
                .iter()
                .all(|system| system.cpu.as_ref().unwrap().usage_percent.is_some())
        );
        assert!(!systems[0].cpu.as_ref().unwrap().cores.is_empty());
        assert!(systems[0].memory.as_ref().unwrap().total_bytes > 0);
        for (index, process_sample) in processes.iter().enumerate() {
            assert_eq!(process_sample.processes.len(), 1);
            let process = &process_sample.processes[0];
            assert_eq!(process.pid, std::process::id());
            assert!(process.rss_bytes > 0);
            assert!(!process.threads.is_empty());
            // Other test-runner threads may exit between directory listing and stat reads.
            if let Some(error) = &process.threads_error {
                assert!(
                    error.ends_with("threads exited or were unreadable"),
                    "{error}"
                );
            }
            assert_eq!(
                process_sample.header.as_ref().unwrap().batch_id,
                index as u64
            );
            assert!(
                process_sample
                    .header
                    .as_ref()
                    .unwrap()
                    .inputs
                    .iter()
                    .all(|input| input.error.is_none())
            );
            assert_eq!(gpus[index].utilization_percent, Some(37.5));
            assert_eq!(gpus[index].shared_system_memory, systems[index].memory);
            assert!(gpus[index].dedicated_memory_used_bytes.is_none());
        }
        assert!(
            processes[1..]
                .iter()
                .any(|sample| sample.processes[0].cpu_percent.unwrap_or(0.0) > 0.0)
        );
        assert!(
            processes[1..]
                .iter()
                .flat_map(|sample| &sample.processes[0].threads)
                .any(|thread| thread.cpu_percent.unwrap_or(0.0) > 0.0)
        );
    }
}
