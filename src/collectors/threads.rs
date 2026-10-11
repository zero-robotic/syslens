use super::procfs::{Selector, TickTracker, ids, read_stat, ticks_per_second};
use crate::{
    data::{ThreadData, ThreadGroup, Threads},
    framework::{Collector, OperationContext},
};
use anyhow::{Result, ensure};
use std::{collections::HashSet, path::PathBuf, time::Instant};

pub struct ThreadCollector {
    root: PathBuf,
    selector: Selector,
    tracker: TickTracker<(u32, u64, u32, u64)>,
}
impl ThreadCollector {
    pub fn new(root: PathBuf, selector: Selector) -> Result<Self> {
        Ok(Self {
            root,
            selector,
            tracker: TickTracker::new(ticks_per_second()?),
        })
    }
}
impl Collector for ThreadCollector {
    type Data = Threads;
    fn collect(&mut self, context: &OperationContext) -> Result<Threads> {
        let mut data = Threads::default();
        let mut live = HashSet::new();
        for pid in self.selector.candidates(&self.root)? {
            context.check()?;
            let path = self.root.join(pid.to_string());
            let Ok(process) = read_stat(&path.join("stat")) else {
                continue;
            };
            if !self.selector.includes(&process) {
                continue;
            }
            let mut group = ThreadGroup {
                process_name: process.name.clone(),
                pid,
                process_start_ticks: process.start_ticks,
                items: Vec::new(),
                error: None,
            };
            let result: Result<()> = (|| {
                let mut skipped = 0;
                for tid in ids(&path.join("task"))? {
                    context.check()?;
                    let stat =
                        match read_stat(&path.join("task").join(tid.to_string()).join("stat")) {
                            Ok(stat) if stat.pid == tid => stat,
                            _ => {
                                skipped += 1;
                                continue;
                            }
                        };
                    let identity = (pid, process.start_ticks, tid, stat.start_ticks);
                    live.insert(identity);
                    group.items.push(ThreadData {
                        tid,
                        start_ticks: stat.start_ticks,
                        name: stat.name,
                        cpu_percent: self.tracker.sample(identity, stat.ticks, Instant::now()),
                        cpu_ticks: stat.ticks,
                    });
                }
                ensure!(
                    read_stat(&path.join("stat"))?.start_ticks == process.start_ticks,
                    "process identity changed during thread scan"
                );
                if skipped > 0 {
                    group.error = Some(format!("{skipped} threads exited or were unreadable"));
                }
                Ok(())
            })();
            if let Err(error) = result {
                group.items.clear();
                group.error = Some(format!("{error:#}"));
            }
            data.groups.push(group);
        }
        self.tracker.retain(&live);
        Ok(data)
    }
}
