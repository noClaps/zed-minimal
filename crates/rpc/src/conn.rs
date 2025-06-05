use async_tungstenite::tungstenite::Message as WebSocketMessage;
use futures::{SinkExt as _, StreamExt as _};

pub struct Connection {
    pub(crate) tx:
        Box<dyn 'static + Send + Unpin + futures::Sink<WebSocketMessage, Error = anyhow::Error>>,
    pub(crate) rx:
        Box<dyn 'static + Send + Unpin + futures::Stream<Item = anyhow::Result<WebSocketMessage>>>,
}

impl Connection {
    pub fn new<S>(stream: S) -> Self
    where
        S: 'static
            + Send
            + Unpin
            + futures::Sink<WebSocketMessage, Error = anyhow::Error>
            + futures::Stream<Item = anyhow::Result<WebSocketMessage>>,
    {
        let (tx, rx) = stream.split();
        Self {
            tx: Box::new(tx),
            rx: Box::new(rx),
        }
    }

    pub async fn send(&mut self, message: WebSocketMessage) -> anyhow::Result<()> {
        self.tx.send(message).await
    }
}
