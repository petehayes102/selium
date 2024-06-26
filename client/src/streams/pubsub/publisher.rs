use super::states::{PublisherWantsEncoder, PublisherWantsOpen};
use crate::batching::{BatchConfig, MessageBatch};
use crate::connection::{ClientConnection, SharedConnection};
use crate::keep_alive::pubsub::KeepAlive;
use crate::keep_alive::AttemptFut;
use crate::streams::aliases::Comp;
use crate::streams::handle_reply;
use crate::traits::{KeepAliveStream, Open, Operations, Retain, TryIntoU64};
use crate::{Client, StreamBuilder};
use async_trait::async_trait;
use bytes::Bytes;
use futures::{Sink, SinkExt};
use selium_protocol::utils::encode_message_batch;
use selium_protocol::{BatchPayload, BiStream, Frame, MessagePayload, PublisherPayload, TopicName};
use selium_std::errors::{CodecError, Result, SeliumError};
use selium_std::traits::codec::MessageEncoder;
use selium_std::traits::compression::Compress;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::sync::MutexGuard;

impl StreamBuilder<PublisherWantsEncoder> {
    /// Specifies the encoder a [Publisher] uses for encoding produced messages prior to being
    /// sent over the wire.
    ///
    /// An encoder can be any type implementing
    /// [MessageEncoder](crate::std::traits::codec::MessageEncoder).
    pub fn with_encoder<E>(self, encoder: E) -> StreamBuilder<PublisherWantsOpen<E>> {
        let next_state = PublisherWantsOpen::new(self.state, encoder);

        StreamBuilder {
            state: next_state,
            client: self.client,
        }
    }
}

impl<E> StreamBuilder<PublisherWantsOpen<E>> {
    /// Specifies the compression implementation a [Publisher] uses for compressing encoded
    /// messages prior to being sent over the wire.
    ///
    /// If message batching is enabled for the stream, the message batch will be compressed as a
    /// single unit, rather than each message being compressed individually.
    ///
    /// A compressor can be any type implementing [Compress](crate::std::traits::compression::Compress).
    pub fn with_compression<T>(mut self, comp: T) -> StreamBuilder<PublisherWantsOpen<E>>
    where
        T: Compress + Send + Sync + 'static,
    {
        self.state.compression = Some(Arc::new(comp));
        self
    }

    /// Enables message batching for a [Publisher] stream.
    ///
    /// Relies on the specified [BatchConfig](crate::batching::BatchConfig) to tune the batching
    /// algorithm.
    ///
    /// When opted in for a stream, batching will happen automatically without any
    /// additional intervention.
    pub fn with_batching(mut self, config: BatchConfig) -> StreamBuilder<PublisherWantsOpen<E>> {
        self.state.batch_config = Some(config);
        self
    }
}

impl<E> Retain for StreamBuilder<PublisherWantsOpen<E>> {
    fn retain<T: TryIntoU64>(mut self, policy: T) -> Result<Self> {
        self.state.common.retain(policy)?;
        Ok(self)
    }
}

impl<E> Operations for StreamBuilder<PublisherWantsOpen<E>> {
    fn map(mut self, module_path: &str) -> Self {
        self.state.common.map(module_path);
        self
    }

    fn filter(mut self, module_path: &str) -> Self {
        self.state.common.filter(module_path);
        self
    }
}

#[async_trait]
impl<E> Open for StreamBuilder<PublisherWantsOpen<E>>
where
    E: MessageEncoder + Clone + Send + Unpin,
{
    type Output = KeepAlive<Publisher<E>>;

    async fn open(self) -> Result<Self::Output> {
        let topic = TopicName::try_from(self.state.common.topic.as_str())?;

        let headers = PublisherPayload {
            topic,
            retention_policy: self.state.common.retention_policy,
            operations: self.state.common.operations,
        };

        let publisher = Publisher::spawn(
            self.client,
            headers,
            self.state.encoder,
            self.state.compression,
            self.state.batch_config,
        )
        .await?;

        Ok(publisher)
    }
}

/// A traditional publisher stream that produces and sends messages to a topic.
///
/// A Publisher is different to a [Subscriber](crate::streams::pubsub::Subscriber), in that it holds a reference
/// to the client connection handle in order to facilitate duplicating the stream. This makes it
/// possible to spawn branching streams to concurrently publish messages to the same topic.
///
/// The Publisher struct implements the [futures::Sink] trait, and can thus be used in the same
/// contexts as a [Sink](futures::Sink). Any messages sent to the sink will be encoded with the
/// provided encoder, before being sent over the wire.
///
/// Publishers are asynchronous, meaning that when a message is sent over the wire, the Publisher
/// will move on without expecting a response, and likewise, won't confirm if the message was delivered
/// to the [Subscriber](crate::streams::pubsub::Subscriber) streams. If you prefer synchronous messaging patterns like RPC,
/// the [Request/Reply](crate::streams::request_reply) streams are an implementation of this pattern.
///
/// **Note:** The Publisher struct is never constructed directly, but rather, via a
/// [StreamBuilder](crate::StreamBuilder).
pub struct Publisher<E> {
    client: Client,
    stream: BiStream,
    headers: PublisherPayload,
    encoder: E,
    compression: Option<Comp>,
    batch: Option<MessageBatch>,
    batch_config: Option<BatchConfig>,
}

impl<E> Publisher<E>
where
    E: MessageEncoder + Clone + Send + Unpin,
{
    async fn spawn(
        client: Client,
        headers: PublisherPayload,
        encoder: E,
        compression: Option<Comp>,
        batch_config: Option<BatchConfig>,
    ) -> Result<KeepAlive<Self>> {
        let batch = batch_config.as_ref().map(|c| MessageBatch::from(c.clone()));
        let lock = client.connection.lock().await;
        let stream = Self::open_stream(lock, headers.clone()).await?;

        let publisher = Self {
            client: client.clone(),
            stream,
            headers,
            encoder,
            compression,
            batch,
            batch_config,
        };

        Ok(KeepAlive::new(publisher, client.backoff_strategy))
    }

    /// Spawns a new [Publisher] stream with the same configuration as the current stream, without
    /// having to specify the same configuration options again.
    ///
    /// This method is especially convenient in cases where multiple streams will be concurrently
    /// publishing messages to the same topic, via cooperative-multitasking, e.g. within [tokio] tasks
    /// spawned by [tokio::spawn].
    ///
    /// See the included examples in the repository for more information.
    ///
    /// # Errors
    ///
    /// Returns [Err] if a new stream cannot be opened on the current client connection.
    pub async fn duplicate(&self) -> Result<KeepAlive<Self>> {
        let publisher = Publisher::spawn(
            self.client.clone(),
            self.headers.clone(),
            self.encoder.clone(),
            self.compression.clone(),
            self.batch_config.clone(),
        )
        .await?;

        Ok(publisher)
    }

    /// Gracefully closes the stream.
    ///
    /// It is highly recommended to invoke this method after no new messages will be
    /// published to this stream. This is to ensure that the `Selium` server has acknowledged all
    /// sent data prior to closing the connection.
    ///
    /// Under the hood, `finish` calls the [finish](quinn::SendStream::finish) on the underlying
    /// [SendStream](quinn::SendStream).
    ///
    /// # Errors
    ///
    /// Returns [Err] if the stream fails to close gracefully.
    pub async fn finish(mut self) -> Result<()> {
        self.flush_batch()?;
        self.stream.finish().await
    }

    async fn open_stream(
        connection: MutexGuard<'_, ClientConnection>,
        headers: PublisherPayload,
    ) -> Result<BiStream> {
        let mut stream = BiStream::try_from_connection(connection.conn()).await?;
        drop(connection);

        let frame = Frame::RegisterPublisher(headers);
        stream.send(frame).await?;

        handle_reply(&mut stream).await?;
        Ok(stream)
    }

    fn send_single(&mut self, mut bytes: Bytes) -> Result<()> {
        if let Some(comp) = &self.compression {
            bytes = comp.compress(bytes).map_err(CodecError::CompressFailure)?;
        }

        let frame = Frame::Message(MessagePayload {
            headers: None,
            message: bytes,
        });
        self.stream.start_send_unpin(frame)
    }

    fn send_batch(&mut self, now: Instant) -> Result<()> {
        let batch = self.batch.as_mut().unwrap();

        let messages = batch.drain();
        let batch_size = messages.len();
        let mut bytes = encode_message_batch(messages);

        if let Some(comp) = &self.compression {
            bytes = comp.compress(bytes).map_err(CodecError::CompressFailure)?;
        }

        let frame = Frame::BatchMessage(BatchPayload {
            size: batch_size as u32,
            message: bytes,
        });

        self.stream.start_send_unpin(frame)?;
        batch.update_last_run(now);

        Ok(())
    }

    fn flush_batch(&mut self) -> Result<()> {
        if let Some(batch) = self.batch.as_ref() {
            if !batch.is_empty() {
                self.send_batch(Instant::now())?;
            }
        }

        Ok(())
    }
}

impl<E> Sink<E::Item> for Publisher<E>
where
    E: MessageEncoder + Clone + Send + Unpin,
{
    type Error = SeliumError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Some(batch) = self.batch.as_ref() {
            let now = Instant::now();

            if batch.is_ready(now) {
                self.send_batch(now)?;
            }

            return Poll::Ready(Ok(()));
        }

        self.stream.poll_ready_unpin(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: E::Item) -> Result<(), Self::Error> {
        let bytes = self
            .encoder
            .encode(item)
            .map_err(CodecError::EncodeFailure)?;

        if let Some(batch) = self.batch.as_mut() {
            batch.push(bytes);
            Ok(())
        } else {
            self.send_single(bytes)
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.stream.poll_flush_unpin(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.stream.poll_close_unpin(cx)
    }
}

impl<E> KeepAliveStream for Publisher<E>
where
    E: MessageEncoder + Clone + Send + Unpin,
{
    type Headers = PublisherPayload;

    fn reestablish_connection(connection: SharedConnection, headers: Self::Headers) -> AttemptFut {
        Box::pin(async move {
            let mut connection = connection.lock().await;
            connection.reconnect().await?;
            Self::open_stream(connection, headers).await
        })
    }

    fn on_reconnect(&mut self, stream: BiStream) {
        self.stream = stream;
    }

    fn get_connection(&self) -> SharedConnection {
        self.client.connection.clone()
    }

    fn get_headers(&self) -> Self::Headers {
        self.headers.clone()
    }
}
