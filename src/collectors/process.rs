pub use super::procfs::Selector;
use super::procfs::{TickTracker, page_size, read_stat, ticks_per_second};
use crate::{
    data::{ProcessData, ProcessMemory, ProcessMemoryData, Processes},
    framework::{CollectContext, Collector},
};
use anyhow::{Context, Result};
use std::{collections::HashSet, path::PathBuf, time::Instant};

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
    fn collect(&mut self, _: &CollectContext) -> Result<Processes> {
        let mut data = Processes::default();
        let mut live = HashSet::new();
        for pid in self.selector.candidates(&self.root)? {
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
    fn collect(&mut self, _: &CollectContext) -> Result<Self::Data> {
        let mut data = ProcessMemory::default();
        for pid in self.selector.candidates(&self.root)? {
            let stat = match read_stat(&self.root.join(pid.to_string()).join("stat")) {
                Ok(stat) if stat.pid == pid && self.selector.includes(&stat) => stat,
                _ => continue,
            };
            data.items.insert(
                (pid, stat.start_ticks),
                ProcessMemoryData {
                    rss_bytes: stat
                        .rss_pages
                        .checked_mul(self.pages)
                        .context("RSS overflow")?,
                    virtual_bytes: stat.virtual_bytes,
                },
            );
        }
        Ok(data)
    }
}
