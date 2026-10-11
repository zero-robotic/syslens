use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashMap, HashSet},
    fs,
    hash::Hash,
    path::Path,
    time::Instant,
};

#[derive(Debug)]
pub struct Stat {
    pub pid: u32,
    pub name: String,
    pub ticks: u64,
    pub start_ticks: u64,
    pub virtual_bytes: u64,
    pub rss_pages: u64,
}
pub fn parse_stat(text: &str) -> Result<Stat> {
    let open = text.find('(').context("stat comm missing")?;
    let close = text.rfind(')').context("stat comm unterminated")?;
    ensure!(close > open, "invalid stat comm");
    let fields: Vec<_> = text[close + 1..].split_whitespace().collect();
    ensure!(fields.len() >= 22, "truncated stat");
    let utime: u64 = fields[11].parse()?;
    let stime: u64 = fields[12].parse()?;
    let rss: i64 = fields[21].parse()?;
    ensure!(rss >= 0, "negative RSS");
    Ok(Stat {
        pid: text[..open].trim().parse()?,
        name: text[open + 1..close].into(),
        ticks: utime.checked_add(stime).context("CPU time overflow")?,
        start_ticks: fields[19].parse()?,
        virtual_bytes: fields[20].parse()?,
        rss_pages: rss as u64,
    })
}
pub fn read_stat(path: &Path) -> Result<Stat> {
    parse_stat(&fs::read_to_string(path).with_context(|| path.display().to_string())?)
}
pub fn ids(path: &Path) -> Result<Vec<u32>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(path).with_context(|| path.display().to_string())? {
        let entry = entry?;
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            result.push(id);
        }
    }
    result.sort_unstable();
    Ok(result)
}
pub fn ticks_per_second() -> Result<f64> {
    // sysconf has no pointer arguments and does not transfer ownership.
    let value = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    ensure!(value > 0, "cannot query _SC_CLK_TCK");
    Ok(value as f64)
}
pub fn page_size() -> Result<u64> {
    // sysconf has no pointer arguments and does not transfer ownership.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    ensure!(value > 0, "cannot query _SC_PAGESIZE");
    Ok(value as u64)
}

#[derive(Clone, Default)]
pub struct Selector {
    pub pids: Vec<u32>,
    pub names: Vec<String>,
}
impl Selector {
    pub fn includes(&self, stat: &Stat) -> bool {
        (self.pids.is_empty() && self.names.is_empty())
            || self.pids.contains(&stat.pid)
            || self.names.iter().any(|name| name == &stat.name)
    }
    pub fn candidates(&self, root: &Path) -> Result<Vec<u32>> {
        if !self.pids.is_empty() && self.names.is_empty() {
            Ok(self.pids.clone())
        } else {
            ids(root)
        }
    }
}

pub struct TickTracker<K> {
    previous: HashMap<K, (u64, Instant)>,
    hz: f64,
}
impl<K: Eq + Hash> TickTracker<K> {
    pub fn new(hz: f64) -> Self {
        Self {
            previous: HashMap::new(),
            hz,
        }
    }
    pub fn sample(&mut self, key: K, ticks: u64, now: Instant) -> Option<f64> {
        let (old_ticks, old_time) = self.previous.insert(key, (ticks, now))?;
        let delta = ticks.checked_sub(old_ticks)?;
        let elapsed = now.checked_duration_since(old_time)?.as_secs_f64();
        (elapsed > 0.0).then(|| delta as f64 / self.hz / elapsed * 100.0)
    }
    pub fn retain(&mut self, live: &HashSet<K>) {
        self.previous.retain(|key, _| live.contains(key));
    }
}

#[cfg(test)]
pub fn fake_stat(pid: u32, name: &str, ticks: u64, start: u64) -> String {
    let mut fields = vec!["0".to_string(); 22];
    fields[0] = "R".into();
    fields[11] = ticks.to_string();
    fields[19] = start.to_string();
    fields[20] = "8192".into();
    fields[21] = "1".into();
    format!("{pid} ({name}) {}", fields.join(" "))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_names_with_spaces_and_closing_parentheses() {
        let data = parse_stat(&fake_stat(123, "name ) with ( spaces", 300, 17)).unwrap();
        assert_eq!(data.name, "name ) with ( spaces");
        assert_eq!(data.ticks, 300);
        assert_eq!(data.start_ticks, 17);
        assert_eq!(data.rss_pages, 1);
        assert!(parse_stat("123 (x) R 1").is_err());
    }
    #[test]
    fn process_percent_uses_one_core_and_identity_prevents_pid_reuse() {
        let mut tracker = TickTracker::new(100.0);
        let now = Instant::now();
        assert_eq!(tracker.sample((123, 17), 100, now), None);
        assert_eq!(
            tracker.sample((123, 17), 300, now + std::time::Duration::from_secs(1)),
            Some(200.0)
        );
        assert_eq!(
            tracker.sample((123, 18), 3, now + std::time::Duration::from_secs(2)),
            None
        );
    }
}
