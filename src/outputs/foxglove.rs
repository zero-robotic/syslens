use crate::framework::{Output, Sample};
use anyhow::Result;
use foxglove::{
    Context, PartialMetadata, RawChannel, Schema, WebSocketServer, WebSocketServerHandle,
};
use std::{collections::HashMap, sync::Arc};

pub struct FoxgloveOutput {
    context: Arc<Context>,
    server: Option<WebSocketServerHandle>,
    channels: HashMap<String, Arc<RawChannel>>,
}
impl FoxgloveOutput {
    pub fn new(host: &str, port: u16) -> Result<Self> {
        let context = Context::new();
        let server = WebSocketServer::new()
            .name("syslens")
            .context(&context)
            .bind(host, port)
            .message_backlog_size(128)
            .start_blocking()?;
        eprintln!("Foxglove listening on ws://{host}:{}", server.port());
        Ok(Self {
            context,
            server: Some(server),
            channels: HashMap::new(),
        })
    }
}
impl Output for FoxgloveOutput {
    fn write(&mut self, sample: &Sample) -> Result<()> {
        let descriptor = &sample.stream;
        if !self.channels.contains_key(&descriptor.topic) {
            let channel = self
                .context
                .channel_builder(&descriptor.topic)
                .message_encoding(&descriptor.message_encoding)
                .schema(Schema::new(
                    &descriptor.schema_name,
                    &descriptor.schema_encoding,
                    descriptor.schema.to_vec(),
                ))
                .build_raw()?;
            self.channels.insert(descriptor.topic.clone(), channel);
        }
        self.channels[&descriptor.topic].log_with_meta(
            &sample.payload,
            PartialMetadata::with_log_time(sample.captured_at_ns),
        );
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        if let Some(server) = self.server.take() {
            server.stop().wait_blocking();
        }
        self.channels.clear();
        Ok(())
    }
}
impl Drop for FoxgloveOutput {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, pb};
    use prost::Message as _;
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};
    use tungstenite::{Message, client::IntoClientRequest, stream::MaybeTlsStream};

    #[test]
    fn websocket_advertises_protobuf_and_delivers_timestamped_payload() {
        let mut output = FoxgloveOutput::new("127.0.0.1", 0).unwrap();
        let sample = protocol::encode(
            &pb::GpuSample {
                utilization_percent: Some(42.5),
                ..Default::default()
            },
            protocol::stream("/syslens/gpu", "GpuSample"),
            1_234_567_890,
        );
        output.write(&sample).unwrap();
        let port = output.server.as_ref().unwrap().port();
        let mut request = format!("ws://127.0.0.1:{port}")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", "foxglove.sdk.v1".parse().unwrap());
        let (mut socket, _) = tungstenite::connect(request).unwrap();
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
        }
        let channel_id = loop {
            if let Message::Text(text) = socket.read().unwrap() {
                let message: Value = serde_json::from_str(&text).unwrap();
                if message["op"] == "advertise" {
                    let channel = message["channels"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|channel| channel["topic"] == "/syslens/gpu")
                        .unwrap();
                    assert_eq!(channel["encoding"], "protobuf");
                    assert_eq!(channel["schemaEncoding"], "protobuf");
                    assert_eq!(channel["schemaName"], "syslens.v1.GpuSample");
                    assert!(!channel["schema"].as_str().unwrap().is_empty());
                    break channel["id"].as_u64().unwrap();
                }
            }
        };
        socket
            .send(Message::Text(
                json!({
                    "op": "subscribe",
                    "subscriptions": [{"id": 7, "channelId": channel_id}]
                })
                .to_string()
                .into(),
            ))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !output.channels["/syslens/gpu"].has_sinks() {
            assert!(Instant::now() < deadline, "subscription was not registered");
            std::thread::sleep(Duration::from_millis(5));
        }
        output.write(&sample).unwrap();
        loop {
            if let Message::Binary(frame) = socket.read().unwrap() {
                assert_eq!(frame[0], 1); // Foxglove Message Data opcode.
                assert_eq!(u32::from_le_bytes(frame[1..5].try_into().unwrap()), 7);
                assert_eq!(
                    u64::from_le_bytes(frame[5..13].try_into().unwrap()),
                    sample.captured_at_ns
                );
                assert_eq!(&frame[13..], sample.payload.as_ref());
                assert_eq!(
                    pb::GpuSample::decode(&frame[13..])
                        .unwrap()
                        .utilization_percent,
                    Some(42.5)
                );
                break;
            }
        }
        socket.close(None).unwrap();
        drop(socket);
        output.close().unwrap();
    }
}
