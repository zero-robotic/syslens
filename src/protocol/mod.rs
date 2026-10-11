use crate::framework::{Sample, StreamDescriptor};
use bytes::Bytes;
use prost::Message;
use std::sync::Arc;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/syslens.v1.rs"));
}
pub const DESCRIPTOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/syslens.bin"));

pub fn stream(topic: &str, message: &str) -> Arc<StreamDescriptor> {
    Arc::new(StreamDescriptor {
        topic: topic.into(),
        message_encoding: "protobuf".into(),
        schema_name: format!("syslens.v1.{message}"),
        schema_encoding: "protobuf".into(),
        schema: Bytes::from_static(DESCRIPTOR),
    })
}
pub fn timestamp(ns: u64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: (ns / 1_000_000_000) as i64,
        nanos: (ns % 1_000_000_000) as i32,
    }
}
pub fn encode(
    message: &impl Message,
    stream: Arc<StreamDescriptor>,
    captured_at_ns: u64,
) -> Sample {
    Sample {
        stream,
        captured_at_ns,
        sequence: 0,
        payload: Bytes::from(message.encode_to_vec()),
    }
}
