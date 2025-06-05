use super::{
    Connection,
    message_stream::{Message, MessageStream},
    proto::{
        self, AnyTypedEnvelope, EnvelopedMessage, PeerId, Receipt, RequestMessage, TypedEnvelope,
    },
};
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::{
    FutureExt, SinkExt, Stream, StreamExt, TryFutureExt,
    channel::{mpsc, oneshot},
    stream::BoxStream,
};
use parking_lot::{Mutex, RwLock};
use proto::{ErrorCode, ErrorCodeExt, ErrorExt, RpcError};
use serde::{Serialize, ser::SerializeStruct};
use std::{
    fmt, future,
    future::Future,
    sync::atomic::Ordering::SeqCst,
    sync::{
        Arc,
        atomic::{self, AtomicU32},
    },
    time::Duration,
    time::Instant,
};
use tracing::instrument;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize)]
pub struct ConnectionId {
    pub owner_id: u32,
    pub id: u32,
}

impl From<ConnectionId> for PeerId {
    fn from(id: ConnectionId) -> Self {
        PeerId {
            owner_id: id.owner_id,
            id: id.id,
        }
    }
}

impl From<PeerId> for ConnectionId {
    fn from(peer_id: PeerId) -> Self {
        Self {
            owner_id: peer_id.owner_id,
            id: peer_id.id,
        }
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner_id, self.id)
    }
}

pub struct Peer {
    epoch: AtomicU32,
    pub connections: RwLock<HashMap<ConnectionId, ConnectionState>>,
    next_connection_id: AtomicU32,
}

#[derive(Clone, Serialize)]
pub struct ConnectionState {
    #[serde(skip)]
    outgoing_tx: mpsc::UnboundedSender<Message>,
    next_message_id: Arc<AtomicU32>,
    #[allow(clippy::type_complexity)]
    #[serde(skip)]
    response_channels: Arc<
        Mutex<
            Option<
                HashMap<
                    u32,
                    oneshot::Sender<(proto::Envelope, std::time::Instant, oneshot::Sender<()>)>,
                >,
            >,
        >,
    >,
    #[allow(clippy::type_complexity)]
    #[serde(skip)]
    stream_response_channels: Arc<
        Mutex<
            Option<
                HashMap<u32, mpsc::UnboundedSender<(Result<proto::Envelope>, oneshot::Sender<()>)>>,
            >,
        >,
    >,
}

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
pub const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);

impl Peer {
    pub fn new(epoch: u32) -> Arc<Self> {
        Arc::new(Self {
            epoch: AtomicU32::new(epoch),
            connections: Default::default(),
            next_connection_id: Default::default(),
        })
    }

    pub fn epoch(&self) -> u32 {
        self.epoch.load(SeqCst)
    }

    #[instrument(skip_all)]
    pub fn add_connection<F, Fut, Out>(
        self: &Arc<Self>,
        connection: Connection,
        create_timer: F,
    ) -> (
        ConnectionId,
        impl Future<Output = anyhow::Result<()>> + Send + use<F, Fut, Out>,
        BoxStream<'static, Box<dyn AnyTypedEnvelope>>,
    )
    where
        F: Send + Fn(Duration) -> Fut,
        Fut: Send + Future<Output = Out>,
        Out: Send,
    {
        // For outgoing messages, use an unbounded channel so that application code
        // can always send messages without yielding. For incoming messages, use a
        // bounded channel so that other peers will receive backpressure if they send
        // messages faster than this peer can process them.
        const INCOMING_BUFFER_SIZE: usize = 256;
        let (mut incoming_tx, incoming_rx) = mpsc::channel(INCOMING_BUFFER_SIZE);
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded();

        let connection_id = ConnectionId {
            owner_id: self.epoch.load(SeqCst),
            id: self.next_connection_id.fetch_add(1, SeqCst),
        };
        let connection_state = ConnectionState {
            outgoing_tx,
            next_message_id: Default::default(),
            response_channels: Arc::new(Mutex::new(Some(Default::default()))),
            stream_response_channels: Arc::new(Mutex::new(Some(Default::default()))),
        };
        let mut writer = MessageStream::new(connection.tx);
        let mut reader = MessageStream::new(connection.rx);

        let this = self.clone();
        let response_channels = connection_state.response_channels.clone();
        let stream_response_channels = connection_state.stream_response_channels.clone();

        let handle_io = async move {
            tracing::trace!(%connection_id, "handle io future: start");

            let _end_connection = util::defer(|| {
                response_channels.lock().take();
                if let Some(channels) = stream_response_channels.lock().take() {
                    for channel in channels.values() {
                        let _ = channel.unbounded_send((
                            Err(anyhow!("connection closed")),
                            oneshot::channel().0,
                        ));
                    }
                }
                this.connections.write().remove(&connection_id);
                tracing::trace!(%connection_id, "handle io future: end");
            });

            // Send messages on this frequency so the connection isn't closed.
            let keepalive_timer = create_timer(KEEPALIVE_INTERVAL).fuse();
            futures::pin_mut!(keepalive_timer);

            // Disconnect if we don't receive messages at least this frequently.
            let receive_timeout = create_timer(RECEIVE_TIMEOUT).fuse();
            futures::pin_mut!(receive_timeout);

            loop {
                tracing::trace!(%connection_id, "outer loop iteration start");
                let read_message = reader.read().fuse();
                futures::pin_mut!(read_message);

                loop {
                    tracing::trace!(%connection_id, "inner loop iteration start");
                    futures::select_biased! {
                        outgoing = outgoing_rx.next().fuse() => match outgoing {
                            Some(outgoing) => {
                                tracing::trace!(%connection_id, "outgoing rpc message: writing");
                                futures::select_biased! {
                                    result = writer.write(outgoing).fuse() => {
                                        tracing::trace!(%connection_id, "outgoing rpc message: done writing");
                                        result.context("failed to write RPC message")?;
                                        tracing::trace!(%connection_id, "keepalive interval: resetting after sending message");
                                        keepalive_timer.set(create_timer(KEEPALIVE_INTERVAL).fuse());
                                    }
                                    _ = create_timer(WRITE_TIMEOUT).fuse() => {
                                        tracing::trace!(%connection_id, "outgoing rpc message: writing timed out");
                                        anyhow::bail!("timed out writing message");
                                    }
                                }
                            }
                            None => {
                                tracing::trace!(%connection_id, "outgoing rpc message: channel closed");
                                return Ok(())
                            },
                        },
                        _ = keepalive_timer => {
                            tracing::trace!(%connection_id, "keepalive interval: pinging");
                            futures::select_biased! {
                                result = writer.write(Message::Ping).fuse() => {
                                    tracing::trace!(%connection_id, "keepalive interval: done pinging");
                                    result.context("failed to send keepalive")?;
                                    tracing::trace!(%connection_id, "keepalive interval: resetting after pinging");
                                    keepalive_timer.set(create_timer(KEEPALIVE_INTERVAL).fuse());
                                }
                                _ = create_timer(WRITE_TIMEOUT).fuse() => {
                                    tracing::trace!(%connection_id, "keepalive interval: pinging timed out");
                                    anyhow::bail!("timed out sending keepalive");
                                }
                            }
                        }
                        incoming = read_message => {
                            let incoming = incoming.context("error reading rpc message from socket")?;
                            tracing::trace!(%connection_id, "incoming rpc message: received");
                            tracing::trace!(%connection_id, "receive timeout: resetting");
                            receive_timeout.set(create_timer(RECEIVE_TIMEOUT).fuse());
                            if let (Message::Envelope(incoming), received_at) = incoming {
                                tracing::trace!(%connection_id, "incoming rpc message: processing");
                                futures::select_biased! {
                                    result = incoming_tx.send((incoming, received_at)).fuse() => match result {
                                        Ok(_) => {
                                            tracing::trace!(%connection_id, "incoming rpc message: processed");
                                        }
                                        Err(_) => {
                                            tracing::trace!(%connection_id, "incoming rpc message: channel closed");
                                            return Ok(())
                                        }
                                    },
                                    _ = create_timer(WRITE_TIMEOUT).fuse() => {
                                        tracing::trace!(%connection_id, "incoming rpc message: processing timed out");
                                        anyhow::bail!("timed out processing incoming message");
                                    }
                                }
                            }
                            break;
                        },
                        _ = receive_timeout => {
                            tracing::trace!(%connection_id, "receive timeout: delay between messages too long");
                            anyhow::bail!("delay between messages too long");
                        }
                    }
                }
            }
        };

        let response_channels = connection_state.response_channels.clone();
        let stream_response_channels = connection_state.stream_response_channels.clone();
        self.connections
            .write()
            .insert(connection_id, connection_state);

        let incoming_rx = incoming_rx.filter_map(move |(incoming, received_at)| {
            let response_channels = response_channels.clone();
            let stream_response_channels = stream_response_channels.clone();
            async move {
                let message_id = incoming.id;
                tracing::trace!(?incoming, "incoming message future: start");
                let _end = util::defer(move || {
                    tracing::trace!(%connection_id, message_id, "incoming message future: end");
                });

                if let Some(responding_to) = incoming.responding_to {
                    tracing::trace!(
                        %connection_id,
                        message_id,
                        responding_to,
                        "incoming response: received"
                    );
                    let response_channel =
                        response_channels.lock().as_mut()?.remove(&responding_to);
                    let stream_response_channel = stream_response_channels
                        .lock()
                        .as_ref()?
                        .get(&responding_to)
                        .cloned();

                    if let Some(tx) = response_channel {
                        let requester_resumed = oneshot::channel();
                        if let Err(error) = tx.send((incoming, received_at, requester_resumed.0)) {
                            tracing::trace!(
                                %connection_id,
                                message_id,
                                responding_to = responding_to,
                                ?error,
                                "incoming response: request future dropped",
                            );
                        }

                        tracing::trace!(
                            %connection_id,
                            message_id,
                            responding_to,
                            "incoming response: waiting to resume requester"
                        );
                        let _ = requester_resumed.1.await;
                        tracing::trace!(
                            %connection_id,
                            message_id,
                            responding_to,
                            "incoming response: requester resumed"
                        );
                    } else if let Some(tx) = stream_response_channel {
                        let requester_resumed = oneshot::channel();
                        if let Err(error) = tx.unbounded_send((Ok(incoming), requester_resumed.0)) {
                            tracing::debug!(
                                %connection_id,
                                message_id,
                                responding_to = responding_to,
                                ?error,
                                "incoming stream response: request future dropped",
                            );
                        }

                        tracing::debug!(
                            %connection_id,
                            message_id,
                            responding_to,
                            "incoming stream response: waiting to resume requester"
                        );
                        let _ = requester_resumed.1.await;
                        tracing::debug!(
                            %connection_id,
                            message_id,
                            responding_to,
                            "incoming stream response: requester resumed"
                        );
                    } else {
                        let message_type = proto::build_typed_envelope(
                            connection_id.into(),
                            received_at,
                            incoming,
                        )
                        .map(|p| p.payload_type_name());
                        tracing::warn!(
                            %connection_id,
                            message_id,
                            responding_to,
                            message_type,
                            "incoming response: unknown request"
                        );
                    }

                    None
                } else {
                    tracing::trace!(%connection_id, message_id, "incoming message: received");
                    proto::build_typed_envelope(connection_id.into(), received_at, incoming)
                        .or_else(|| {
                            tracing::error!(
                                %connection_id,
                                message_id,
                                "unable to construct a typed envelope"
                            );
                            None
                        })
                }
            }
        });
        (connection_id, handle_io, incoming_rx.boxed())
    }

    pub fn disconnect(&self, connection_id: ConnectionId) {
        self.connections.write().remove(&connection_id);
    }

    pub fn teardown(&self) {
        self.connections.write().clear();
    }

    /// Make a request and wait for a response.
    pub fn request<T: RequestMessage>(
        &self,
        receiver_id: ConnectionId,
        request: T,
    ) -> impl Future<Output = Result<T::Response>> + use<T> {
        self.request_internal(None, receiver_id, request)
            .map_ok(|envelope| envelope.payload)
    }

    pub fn request_envelope<T: RequestMessage>(
        &self,
        receiver_id: ConnectionId,
        request: T,
    ) -> impl Future<Output = Result<TypedEnvelope<T::Response>>> + use<T> {
        self.request_internal(None, receiver_id, request)
    }

    pub fn forward_request<T: RequestMessage>(
        &self,
        sender_id: ConnectionId,
        receiver_id: ConnectionId,
        request: T,
    ) -> impl Future<Output = Result<T::Response>> {
        self.request_internal(Some(sender_id), receiver_id, request)
            .map_ok(|envelope| envelope.payload)
    }

    fn request_internal<T: RequestMessage>(
        &self,
        original_sender_id: Option<ConnectionId>,
        receiver_id: ConnectionId,
        request: T,
    ) -> impl Future<Output = Result<TypedEnvelope<T::Response>>> + use<T> {
        let envelope = request.into_envelope(0, None, original_sender_id.map(Into::into));
        let response = self.request_dynamic(receiver_id, envelope, T::NAME);
        async move {
            let (response, received_at) = response.await?;
            Ok(TypedEnvelope {
                message_id: response.id,
                sender_id: receiver_id.into(),
                original_sender_id: response.original_sender_id,
                payload: T::Response::from_envelope(response)
                    .context("received response of the wrong type")?,
                received_at,
            })
        }
    }

    /// Make a request and wait for a response.
    ///
    /// The caller must make sure to deserialize the response into the request's
    /// response type. This interface is only useful in trait objects, where
    /// generics can't be used. If you have a concrete type, use `request`.
    pub fn request_dynamic(
        &self,
        receiver_id: ConnectionId,
        mut envelope: proto::Envelope,
        type_name: &'static str,
    ) -> impl Future<Output = Result<(proto::Envelope, Instant)>> + use<> {
        let (tx, rx) = oneshot::channel();
        let send = self.connection_state(receiver_id).and_then(|connection| {
            envelope.id = connection.next_message_id.fetch_add(1, SeqCst);
            connection
                .response_channels
                .lock()
                .as_mut()
                .context("connection was closed")?
                .insert(envelope.id, tx);
            connection
                .outgoing_tx
                .unbounded_send(Message::Envelope(envelope))
                .context("connection was closed")?;
            Ok(())
        });
        async move {
            send?;
            let (response, received_at, _barrier) = rx.await.context("connection was closed")?;
            if let Some(proto::envelope::Payload::Error(error)) = &response.payload {
                return Err(RpcError::from_proto(error, type_name));
            }
            Ok((response, received_at))
        }
    }

    pub fn request_stream<T: RequestMessage>(
        &self,
        receiver_id: ConnectionId,
        request: T,
    ) -> impl Future<Output = Result<impl Unpin + Stream<Item = Result<T::Response>>>> {
        let (tx, rx) = mpsc::unbounded();
        let send = self.connection_state(receiver_id).and_then(|connection| {
            let message_id = connection.next_message_id.fetch_add(1, SeqCst);
            let stream_response_channels = connection.stream_response_channels.clone();
            stream_response_channels
                .lock()
                .as_mut()
                .context("connection was closed")?
                .insert(message_id, tx);
            connection
                .outgoing_tx
                .unbounded_send(Message::Envelope(
                    request.into_envelope(message_id, None, None),
                ))
                .context("connection was closed")?;
            Ok((message_id, stream_response_channels))
        });

        async move {
            let (message_id, stream_response_channels) = send?;
            let stream_response_channels = Arc::downgrade(&stream_response_channels);

            Ok(rx.filter_map(move |(response, _barrier)| {
                let stream_response_channels = stream_response_channels.clone();
                future::ready(match response {
                    Ok(response) => {
                        if let Some(proto::envelope::Payload::Error(error)) = &response.payload {
                            Some(Err(RpcError::from_proto(error, T::NAME)))
                        } else if let Some(proto::envelope::Payload::EndStream(_)) =
                            &response.payload
                        {
                            // Remove the transmitting end of the response channel to end the stream.
                            if let Some(channels) = stream_response_channels.upgrade() {
                                if let Some(channels) = channels.lock().as_mut() {
                                    channels.remove(&message_id);
                                }
                            }
                            None
                        } else {
                            Some(
                                T::Response::from_envelope(response)
                                    .context("received response of the wrong type"),
                            )
                        }
                    }
                    Err(error) => Some(Err(error)),
                })
            }))
        }
    }

    pub fn send<T: EnvelopedMessage>(&self, receiver_id: ConnectionId, message: T) -> Result<()> {
        let connection = self.connection_state(receiver_id)?;
        let message_id = connection
            .next_message_id
            .fetch_add(1, atomic::Ordering::SeqCst);
        connection.outgoing_tx.unbounded_send(Message::Envelope(
            message.into_envelope(message_id, None, None),
        ))?;
        Ok(())
    }

    pub fn send_dynamic(&self, receiver_id: ConnectionId, message: proto::Envelope) -> Result<()> {
        let connection = self.connection_state(receiver_id)?;
        connection
            .outgoing_tx
            .unbounded_send(Message::Envelope(message))?;
        Ok(())
    }

    pub fn forward_send<T: EnvelopedMessage>(
        &self,
        sender_id: ConnectionId,
        receiver_id: ConnectionId,
        message: T,
    ) -> Result<()> {
        let connection = self.connection_state(receiver_id)?;
        let message_id = connection
            .next_message_id
            .fetch_add(1, atomic::Ordering::SeqCst);
        connection
            .outgoing_tx
            .unbounded_send(Message::Envelope(message.into_envelope(
                message_id,
                None,
                Some(sender_id.into()),
            )))?;
        Ok(())
    }

    pub fn respond<T: RequestMessage>(
        &self,
        receipt: Receipt<T>,
        response: T::Response,
    ) -> Result<()> {
        let connection = self.connection_state(receipt.sender_id.into())?;
        let message_id = connection
            .next_message_id
            .fetch_add(1, atomic::Ordering::SeqCst);
        connection
            .outgoing_tx
            .unbounded_send(Message::Envelope(response.into_envelope(
                message_id,
                Some(receipt.message_id),
                None,
            )))?;
        Ok(())
    }

    pub fn end_stream<T: RequestMessage>(&self, receipt: Receipt<T>) -> Result<()> {
        let connection = self.connection_state(receipt.sender_id.into())?;
        let message_id = connection
            .next_message_id
            .fetch_add(1, atomic::Ordering::SeqCst);

        let message = proto::EndStream {};

        connection
            .outgoing_tx
            .unbounded_send(Message::Envelope(message.into_envelope(
                message_id,
                Some(receipt.message_id),
                None,
            )))?;
        Ok(())
    }

    pub fn respond_with_error<T: RequestMessage>(
        &self,
        receipt: Receipt<T>,
        response: proto::Error,
    ) -> Result<()> {
        let connection = self.connection_state(receipt.sender_id.into())?;
        let message_id = connection
            .next_message_id
            .fetch_add(1, atomic::Ordering::SeqCst);
        connection
            .outgoing_tx
            .unbounded_send(Message::Envelope(response.into_envelope(
                message_id,
                Some(receipt.message_id),
                None,
            )))?;
        Ok(())
    }

    pub fn respond_with_unhandled_message(
        &self,
        sender_id: ConnectionId,
        request_message_id: u32,
        message_type_name: &'static str,
    ) -> Result<()> {
        let connection = self.connection_state(sender_id)?;
        let response = ErrorCode::Internal
            .message(format!("message {} was not handled", message_type_name))
            .to_proto();
        let message_id = connection
            .next_message_id
            .fetch_add(1, atomic::Ordering::SeqCst);
        connection
            .outgoing_tx
            .unbounded_send(Message::Envelope(response.into_envelope(
                message_id,
                Some(request_message_id),
                None,
            )))?;
        Ok(())
    }

    fn connection_state(&self, connection_id: ConnectionId) -> Result<ConnectionState> {
        let connections = self.connections.read();
        let connection = connections
            .get(&connection_id)
            .with_context(|| format!("no such connection: {connection_id}"))?;
        Ok(connection.clone())
    }
}

impl Serialize for Peer {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("Peer", 2)?;
        state.serialize_field("connections", &*self.connections.read())?;
        state.end()
    }
}
