pub use super::procfs::Selector;
use super::procfs::{TickTracker, page_size, read_stat, ticks_per_second};
use crate::{
    data::{ProcessData, ProcessMemory, ProcessMemoryData, Processes},
    framework::{Collector, OperationContext},
};
use anyhow::{Context, Result};
use std::{collections::HashSet, fs, path::PathBuf, time::Instant};

pub struct ProcessCollector {
    root: PathBuf,
    selector: Selector,
    tracker: TickTracker<(u32, u64)>,
}
impl ProcessCollector {
    pub fn new(root: PathBuf, selector: Selector) -> Result<Self> {
        Ok(Self {
            root,
            selector,
            tracker: TickTracker::new(ticks_per_second()?),
        })
    }
}
impl Collector for ProcessCollector {
    type Data = Processes;
    fn collect(&mut self, context: &OperationContext) -> Result<Processes> {
        let mut data = Processes::default();
        let mut live = HashSet::new();
        for pid in self.selector.candidates(&self.root)? {
            context.check()?;
            let stat = match read_stat(&self.root.join(pid.to_string()).join("stat")) {
                Ok(stat) if stat.pid == pid => stat,
                _ => {
                    data.skipped += 1;
                    continue;
                }
            };
            if !self.selector.includes(&stat) {
                continue;
            }
            let identity = (pid, stat.start_ticks);
            live.insert(identity);
            let cpu_percent = self.tracker.sample(identity, stat.ticks, Instant::now());
            data.items.push(ProcessData {
                pid,
                start_ticks: stat.start_ticks,
                name: stat.name,
                cpu_percent,
                cpu_ticks: stat.ticks,
            });
        }
        self.tracker.retain(&live);
        Ok(data)
    }
}

pub struct ProcessMemoryCollector {
    root: PathBuf,
    selector: Selector,
    pages: u64,
}
impl ProcessMemoryCollector {
    pub fn new(root: PathBuf, selector: Selector) -> Result<Self> {
        Ok(Self {
            root,
            selector,
            pages: page_size()?,
        })
    }
}
impl Collector for ProcessMemoryCollector {
    type Data = ProcessMemory;
    fn collect(&mut self, context: &OperationContext) -> Result<Self::Data> {
        let mut data = ProcessMemory::default();
        for pid in self.selector.candidates(&self.root)? {
            context.check()?;
            let directory = self.root.join(pid.to_string());
            let stat = match read_stat(&directory.join("stat")) {
                Ok(stat) if stat.pid == pid && self.selector.includes(&stat) => stat,
                _ => continue,
            };
            // Refresh argv as well as memory: exec/argv changes do not change start_ticks.
            // Failure to read argv must not hide otherwise valid memory measurements.
            let (command_line, command_line_error) = match fs::read(directory.join("cmdline")) {
                Ok(bytes) => (Some(format_command_line(&bytes)), None),
                Err(error) => (None, Some(format!("read /proc/{pid}/cmdline: {error}"))),
            };
            // Do not attach a new PID occupant's command line to the old instance.
            match read_stat(&directory.join("stat")) {
                Ok(after) if (after.pid, after.start_ticks) == (pid, stat.start_ticks) => {}
                _ => continue,
            }
            data.items.insert(
                (pid, stat.start_ticks),
                ProcessMemoryData {
                    name: stat.name,
                    rss_bytes: stat
                        .rss_pages
                        .checked_mul(self.pages)
                        .context("RSS overflow")?,
                    virtual_bytes: stat.virtual_bytes,
                    command_line,
                    command_line_error,
                },
            );
        }
        Ok(data)
    }
}

fn format_command_line(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    bytes
        .split(|byte| *byte == 0)
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::procfs::fake_stat;

    #[test]
    fn formats_arguments_without_losing_internal_spaces_or_empty_arguments() {
        assert_eq!(
            format_command_line(b"python3\0worker.py\0--name\0a b\0"),
            "python3 worker.py --name a b"
        );
        assert_eq!(format_command_line(b"app\0\0last"), "app  last");
        assert_eq!(format_command_line(b""), "");
        assert_eq!(format_command_line(b"app\0\xff\0"), "app \u{fffd}");
    }

    #[test]
    fn refreshes_command_line_and_preserves_memory_when_it_is_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("42");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("stat"), fake_stat(42, "worker", 0, 9)).unwrap();
        let mut collector =
            ProcessMemoryCollector::new(root.path().into(), Selector::default()).unwrap();
        let collect = |collector: &mut ProcessMemoryCollector| {
            collector
                .collect(&OperationContext::now())
                .unwrap()
                .items
                .remove(&(42, 9))
                .unwrap()
        };
        fs::write(directory.join("cmdline"), b"worker\0--mode\0one\0").unwrap();
        let row = collect(&mut collector);
        assert_eq!(row.command_line.as_deref(), Some("worker --mode one"));
        assert!(row.command_line_error.is_none());
        fs::write(directory.join("cmdline"), b"worker\0--mode\0two\0").unwrap();
        assert_eq!(
            collect(&mut collector).command_line.as_deref(),
            Some("worker --mode two")
        );
        fs::write(directory.join("cmdline"), b"").unwrap();
        let row = collect(&mut collector);
        assert_eq!(row.command_line.as_deref(), Some(""));
        assert!(row.command_line_error.is_none());
        fs::remove_file(directory.join("cmdline")).unwrap();
        let row = collect(&mut collector);
        assert!(row.command_line.is_none());
        assert!(row.command_line_error.is_some());
        assert!(row.rss_bytes > 0);
        assert_eq!(row.virtual_bytes, 8192);
    }
}
