use crate::{
    data::CpuData,
    framework::{Collector, OperationContext},
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, fs, path::PathBuf};

#[derive(Clone, Copy, Debug)]
struct Counters {
    total: u64,
    idle: u64,
}

fn parse(text: &str) -> Result<BTreeMap<String, Counters>> {
    let mut cpus = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        if name != "cpu"
            && !name
                .strip_prefix("cpu")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        // guest/guest_nice are already included in user/nice. Count only the first 8.
        let ticks = fields
            .take(8)
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()?;
        ensure!(ticks.len() >= 4, "incomplete CPU counters for {name}");
        let total = ticks
            .iter()
            .try_fold(0u64, |sum, value| sum.checked_add(*value))
            .context("CPU counter overflow")?;
        let idle = ticks[3]
            .checked_add(ticks.get(4).copied().unwrap_or(0))
            .context("idle counter overflow")?;
        cpus.insert(name.into(), Counters { total, idle });
    }
    ensure!(cpus.contains_key("cpu"), "missing total CPU counters");
    Ok(cpus)
}
fn utilization(previous: Counters, current: Counters) -> Option<f64> {
    let total = current.total.checked_sub(previous.total)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    (total > 0 && idle <= total).then(|| 100.0 * (total - idle) as f64 / total as f64)
}

pub struct CpuCollector {
    path: PathBuf,
    previous: BTreeMap<String, Counters>,
}
impl CpuCollector {
    pub fn new(proc_root: &std::path::Path) -> Self {
        Self {
            path: proc_root.join("stat"),
            previous: BTreeMap::new(),
        }
    }
}
impl Collector for CpuCollector {
    type Data = CpuData;
    fn collect(&mut self, context: &OperationContext) -> Result<CpuData> {
        context.check()?;
        let current = parse(
            &fs::read_to_string(&self.path).with_context(|| self.path.display().to_string())?,
        )?;
        let mut data = CpuData::default();
        for (name, counters) in &current {
            let percent = self
                .previous
                .get(name)
                .and_then(|old| utilization(*old, *counters));
            if name == "cpu" {
                data.usage_percent = percent;
            } else {
                data.cores.insert(name[3..].parse()?, percent);
            }
        }
        self.previous = current;
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn guest_is_not_double_counted_and_iowait_is_idle() {
        let first = parse("cpu 10 0 10 70 10 0 0 0 8 0\ncpu0 10 0 10 70 10 0 0 0\n").unwrap();
        let second = parse("cpu 30 0 20 110 20 0 0 0 20 0\ncpu0 30 0 20 110 20 0 0 0\n").unwrap();
        assert_eq!(first["cpu"].total, 100);
        assert_eq!(utilization(first["cpu"], second["cpu"]), Some(37.5));
        assert_eq!(utilization(second["cpu"], first["cpu"]), None);
        assert_eq!(utilization(first["cpu"], first["cpu"]), None);
    }
    #[test]
    fn hotplug_and_first_sample_have_no_false_zero() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("stat"), "cpu 1 0 0 9\ncpu0 1 0 0 9\n").unwrap();
        let mut collector = CpuCollector::new(dir.path());
        assert_eq!(
            collector
                .collect(&OperationContext::now())
                .unwrap()
                .usage_percent,
            None
        );
        fs::write(dir.path().join("stat"), "cpu 2 0 0 18\ncpu2 1 0 0 9\n").unwrap();
        let data = collector.collect(&OperationContext::now()).unwrap();
        assert_eq!(data.usage_percent, Some(10.0));
        assert_eq!(data.cores.get(&2), Some(&None));
        assert!(!data.cores.contains_key(&0));
    }
}
