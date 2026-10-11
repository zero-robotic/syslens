use bytes::Bytes;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq)]
pub struct StreamDescriptor {
    pub topic: String,
    pub message_encoding: String,
    pub schema_name: String,
    pub schema_encoding: String,
    pub schema: Bytes,
}

#[derive(Debug, Clone)]
pub struct Sample {
    pub stream: Arc<StreamDescriptor>,
    pub captured_at_ns: u64,
    pub sequence: u32,
    pub payload: Bytes,
}
