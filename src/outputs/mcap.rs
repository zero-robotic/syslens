use crate::framework::{OperationContext, Output, Sample, timestamp_ns};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

pub struct McapOutput {
    directory: PathBuf,
    writer: Option<mcap::Writer<BufWriter<File>>>,
    channels: HashMap<String, u16>,
    started: Instant,
    last_flush: Instant,
    rotate_after: Duration,
    flush_after: Duration,
    payload_limit: u64,
    payload_bytes: u64,
    segment: u64,
}
impl McapOutput {
    pub fn new(
        directory: PathBuf,
        rotate_after: Duration,
        flush_after: Duration,
        payload_limit: u64,
    ) -> Result<Self> {
        fs::create_dir_all(&directory)?;
        let mut result = Self {
            directory,
            writer: None,
            channels: HashMap::new(),
            started: Instant::now(),
            last_flush: Instant::now(),
            rotate_after,
            flush_after,
            payload_limit,
            payload_bytes: 0,
            segment: 0,
        };
        result.open()?;
        Ok(result)
    }
    fn open(&mut self) -> Result<()> {
        let path = self.directory.join(format!(
            "syslens-{}-{}-{:05}.mcap",
            timestamp_ns(),
            std::process::id(),
            self.segment
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| path.display().to_string())?;
        self.writer = Some(mcap::Writer::with_options(
            BufWriter::new(file),
            mcap::WriteOptions::new()
                .compression(Some(mcap::Compression::Zstd))
                .chunk_size(Some(1024 * 1024)),
        )?);
        self.started = Instant::now();
        self.last_flush = self.started;
        self.payload_bytes = 0;
        self.channels.clear();
        self.segment += 1;
        eprintln!("Recording {}", path.display());
        Ok(())
    }
    fn finish_segment(&mut self) -> Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.finish()?;
            let mut buffer = writer.into_inner();
            buffer.flush()?;
            buffer.get_ref().sync_all()?;
        }
        Ok(())
    }
}
impl Output for McapOutput {
    fn write(&mut self, sample: &Sample, context: &OperationContext) -> Result<()> {
        context.check()?;
        if self.payload_bytes > 0
            && (self.started.elapsed() >= self.rotate_after
                || self.payload_bytes >= self.payload_limit)
        {
            self.finish_segment()?;
            self.open()?;
        }
        let writer = self.writer.as_mut().context("MCAP output already closed")?;
        let descriptor = &sample.stream;
        let channel_id = match self.channels.get(&descriptor.topic) {
            Some(id) => *id,
            None => {
                let schema = writer.add_schema(
                    &descriptor.schema_name,
                    &descriptor.schema_encoding,
                    &descriptor.schema,
                )?;
                let id = writer.add_channel(
                    schema,
                    &descriptor.topic,
                    &descriptor.message_encoding,
                    &BTreeMap::new(),
                )?;
                self.channels.insert(descriptor.topic.clone(), id);
                id
            }
        };
        writer.write_to_known_channel(
            &mcap::records::MessageHeader {
                channel_id,
                sequence: sample.sequence,
                log_time: sample.captured_at_ns,
                publish_time: sample.captured_at_ns,
            },
            &sample.payload,
        )?;
        self.payload_bytes += sample.payload.len() as u64;
        if self.last_flush.elapsed() >= self.flush_after {
            writer.flush()?;
            self.last_flush = Instant::now();
        }
        Ok(())
    }
    fn close(&mut self, context: &OperationContext) -> Result<()> {
        context.check()?;
        self.finish_segment()
    }
}
impl Drop for McapOutput {
    fn drop(&mut self) {
        let _ = self.finish_segment();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, pb};
    use prost::Message;
    #[test]
    fn rotated_files_preserve_schema_payload_and_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let mut output = McapOutput::new(
            dir.path().into(),
            Duration::from_secs(3600),
            Duration::from_secs(5),
            1,
        )
        .unwrap();
        let message = pb::SystemSample {
            gpu: Some(pb::GpuMetrics {
                utilization_percent: Some(37.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let sample = protocol::encode(
            &message,
            protocol::stream("/syslens/system", "SystemSample"),
            123456789,
        );
        for _ in 0..3 {
            output.write(&sample, &OperationContext::now()).unwrap();
        }
        output.close(&OperationContext::now()).unwrap();
        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|item| item.unwrap().path())
            .collect();
        assert_eq!(files.len(), 3);
        for file in files {
            let bytes = fs::read(file).unwrap();
            let summary = mcap::Summary::read(&bytes).unwrap().unwrap();
            assert_eq!(summary.stats.unwrap().message_count, 1);
            let messages: Vec<_> = mcap::MessageStream::new(&bytes)
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(messages[0].log_time, 123456789);
            let schema = messages[0].channel.schema.as_ref().unwrap();
            assert_eq!(schema.name, "syslens.v1.SystemSample");
            let descriptor = prost_types::FileDescriptorSet::decode(schema.data.as_ref()).unwrap();
            let schema_files: std::collections::HashSet<_> = descriptor
                .file
                .iter()
                .filter_map(|file| file.name.as_deref())
                .collect();
            assert!(schema_files.contains("google/protobuf/timestamp.proto"));
            for file in &descriptor.file {
                for dependency in &file.dependency {
                    assert!(
                        schema_files.contains(dependency.as_str()),
                        "missing schema import {dependency}"
                    );
                }
            }
            assert_eq!(
                pb::SystemSample::decode(messages[0].data.as_ref())
                    .unwrap()
                    .gpu
                    .unwrap()
                    .utilization_percent,
                Some(37.0)
            );
        }
    }
}
