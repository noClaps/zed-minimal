#![allow(non_snake_case)]

pub use ::proto::*;

use async_tungstenite::tungstenite::Message as WebSocketMessage;
use futures::{SinkExt as _, StreamExt as _};
use proto::Message as _;
use std::time::Instant;
use std::{fmt::Debug, io};
use zstd::zstd_safe::WriteBuf;

const KIB: usize = 1024;
const MIB: usize = KIB * 1024;
const MAX_BUFFER_LEN: usize = MIB;

/// A stream of protobuf messages.
pub struct MessageStream<S> {
    stream: S,
    encoding_buffer: Vec<u8>,
}

#[derive(Debug)]
pub enum Message {
    Envelope(Envelope),
    Ping,
    Pong,
}

impl<S> MessageStream<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            encoding_buffer: Vec::new(),
        }
    }
}

impl<S> MessageStream<S>
where
    S: futures::Sink<WebSocketMessage, Error = anyhow::Error> + Unpin,
{
    pub async fn write(&mut self, message: Message) -> anyhow::Result<()> {
        const COMPRESSION_LEVEL: i32 = 4;

        match message {
            Message::Envelope(message) => {
                self.encoding_buffer.reserve(message.encoded_len());
                message
                    .encode(&mut self.encoding_buffer)
                    .map_err(io::Error::from)?;
                let buffer =
                    zstd::stream::encode_all(self.encoding_buffer.as_slice(), COMPRESSION_LEVEL)
                        .unwrap();

                self.encoding_buffer.clear();
                self.encoding_buffer.shrink_to(MAX_BUFFER_LEN);
                self.stream
                    .send(WebSocketMessage::Binary(buffer.into()))
                    .await?;
            }
            Message::Ping => {
                self.stream
                    .send(WebSocketMessage::Ping(Default::default()))
                    .await?;
            }
            Message::Pong => {
                self.stream
                    .send(WebSocketMessage::Pong(Default::default()))
                    .await?;
            }
        }

        Ok(())
    }
}

impl<S> MessageStream<S>
where
    S: futures::Stream<Item = anyhow::Result<WebSocketMessage>> + Unpin,
{
    pub async fn read(&mut self) -> anyhow::Result<(Message, Instant)> {
        while let Some(bytes) = self.stream.next().await {
            let received_at = Instant::now();
            match bytes? {
                WebSocketMessage::Binary(bytes) => {
                    zstd::stream::copy_decode(bytes.as_slice(), &mut self.encoding_buffer)?;
                    let envelope = Envelope::decode(self.encoding_buffer.as_slice())
                        .map_err(io::Error::from)?;

                    self.encoding_buffer.clear();
                    self.encoding_buffer.shrink_to(MAX_BUFFER_LEN);
                    return Ok((Message::Envelope(envelope), received_at));
                }
                WebSocketMessage::Ping(_) => return Ok((Message::Ping, received_at)),
                WebSocketMessage::Pong(_) => return Ok((Message::Pong, received_at)),
                WebSocketMessage::Close(_) => break,
                _ => {}
            }
        }
        anyhow::bail!("connection closed");
    }
}
