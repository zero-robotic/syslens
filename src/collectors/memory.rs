use crate::{
    data::MemoryData,
    framework::{CollectContext, Collector},
};
use anyhow::{Context, Result, ensure};
use std::{collections::HashMap, fs, path::PathBuf};

pub struct MemoryCollector {
    path: PathBuf,
}
impl MemoryCollector {
    pub fn new(proc_root: &std::path::Path) -> Self {
        Self {
            path: proc_root.join("meminfo"),
        }
    }
}

pub fn parse(text: &str) -> Result<MemoryData> {
    let mut values = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(key), Some(value), Some("kB")) = (fields.next(), fields.next(), fields.next())
        {
            values.insert(
                key.trim_end_matches(':'),
                value
                    .parse::<u64>()?
                    .checked_mul(1024)
                    .context("memory counter overflow")?,
            );
        }
    }
    let total_bytes = *values.get("MemTotal").context("MemTotal unavailable")?;
    let available_bytes = *values
        .get("MemAvailable")
        .context("MemAvailable unavailable (requires Linux 3.14+)")?;
    ensure!(
        available_bytes <= total_bytes,
        "MemAvailable exceeds MemTotal"
    );
    Ok(MemoryData {
        total_bytes,
        available_bytes,
        swap_total_bytes: values.get("SwapTotal").copied().unwrap_or(0),
        swap_free_bytes: values.get("SwapFree").copied().unwrap_or(0),
    })
}
impl Collector for MemoryCollector {
    type Data = MemoryData;
    fn collect(&mut self, _: &CollectContext) -> Result<MemoryData> {
        parse(&fs::read_to_string(&self.path)?)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_units_and_requires_available_memory() {
        let data =
            parse("MemTotal: 4096 kB\nMemAvailable: 1024 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n")
                .unwrap();
        assert_eq!(data.total_bytes, 4194304);
        assert_eq!(data.available_bytes, 1048576);
        assert!(parse("MemTotal: 4096 kB\n").is_err());
    }
}
