use super::states::{SubscriberWantsDecoder, SubscriberWantsOpen};
use crate::connection::{ClientConnection, SharedConnection};
use crate::keep_alive::pubsub::KeepAlive;
use crate::keep_alive::AttemptFut;
use crate::streams::aliases::Decomp;
use crate::streams::handle_reply;
use crate::traits::{KeepAliveStream, Open, Operations, Retain, TryIntoU64};
use crate::{Client, StreamBuilder};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, Stream, StreamExt};
use selium_protocol::utils::decode_message_batch;
use selium_protocol::{BiStream, Frame, Offset, SubscriberPayload, TopicName};
use selium_std::errors::{CodecError, Result};
use selium_std::traits::codec::MessageDecoder;
use selium_std::traits::compression::Decompress;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::MutexGuard;

impl StreamBuilder<SubscriberWantsDecoder> {
    /// Specifies the decoder a [Subscriber] uses for decoding messages received over the wire.
    ///
    /// A decoder can be any type implementing
    /// [MessageDecoder](crate::std::traits::codec::MessageDecoder).
    pub fn with_decoder<D>(self, decoder: D) -> StreamBuilder<SubscriberWantsOpen<D>> {
        let next_state = SubscriberWantsOpen::new(self.state, decoder);

        StreamBuilder {
            state: next_state,
            client: self.client,
        }
    }
}

impl<D> StreamBuilder<SubscriberWantsOpen<D>> {
    /// Specifies the decompression implementation a [Subscriber] uses for
    /// decompressing messages received over the wire prior to decoding.
    ///
    /// A decompressor can be any type implementing
    /// [Decompress](crate::std::traits::compression::Decompress).
    pub fn with_decompression<T>(mut self, decomp: T) -> StreamBuilder<SubscriberWantsOpen<D>>
    where
        T: Decompress + Send + Sync + 'static,
    {
        self.state.decompression = Some(Arc::new(decomp));
        self
    }

    pub fn seek(mut self, offset: Offset) -> Self {
        self.state.offset = offset;
        self
    }
}

impl<D> Retain for StreamBuilder<SubscriberWantsOpen<D>> {
    fn retain<T: TryIntoU64>(mut self, policy: T) -> Result<Self> {
        self.state.common.retain(policy)?;
        Ok(self)
    }
}

impl<D> Operations for StreamBuilder<SubscriberWantsOpen<D>> {
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
impl<D> Open for StreamBuilder<SubscriberWantsOpen<D>>
where
    D: MessageDecoder + Send + Unpin,
{
    type Output = KeepAlive<Subscriber<D>>;

    async fn open(self) -> Result<Self::Output> {
        let topic = TopicName::try_from(self.state.common.topic.as_str())?;

        let headers = SubscriberPayload {
            topic,
            retention_policy: self.state.common.retention_policy,
            operations: self.state.common.operations,
            offset: self.state.offset,
        };

        let subscriber = Subscriber::spawn(
            self.client,
            headers,
            self.state.decoder,
            self.state.decompression,
        )
        .await?;

        Ok(subscriber)
    }
}

/// A traditional subscriber stream that consumes messages produced by a topic.
///
/// The Subscriber struct implements the [futures::Stream] trait, and can thus be used in the same
/// contexts as a [Stream](futures::Stream). Any messages polled on the stream will be decoded
/// using the provided decoder.
///
/// **Note:** The Subscriber struct is never constructed directly, but rather, via a
/// [StreamBuilder](crate::StreamBuilder).
pub struct Subscriber<D> {
    client: Client,
    stream: BiStream,
    headers: SubscriberPayload,
    decoder: D,
    decompression: Option<Decomp>,
    message_batch: Option<Vec<Bytes>>,
}

impl<D> Subscriber<D>
where
    D: MessageDecoder + Send + Unpin,
{
    async fn spawn(
        client: Client,
        headers: SubscriberPayload,
        decoder: D,
        decompression: Option<Decomp>,
    ) -> Result<KeepAlive<Self>> {
        let lock = client.connection.lock().await;
        let stream = Self::open_stream(lock, headers.clone()).await?;

        let subscriber = Self {
            client: client.clone(),
            stream,
            headers,
            decoder,
            message_batch: None,
            decompression,
        };

        Ok(KeepAlive::new(subscriber, client.backoff_strategy))
    }

    async fn open_stream(
        connection: MutexGuard<'_, ClientConnection>,
        headers: SubscriberPayload,
    ) -> Result<BiStream> {
        let mut stream = BiStream::try_from_connection(connection.conn()).await?;
        drop(connection);

        let frame = Frame::RegisterSubscriber(headers);
        stream.send(frame).await?;

        handle_reply(&mut stream).await?;
        Ok(stream)
    }

    fn decode_message(&mut self, bytes: Bytes) -> Poll<Option<Result<D::Item>>> {
        let mut mut_bytes = BytesMut::with_capacity(bytes.len());
        mut_bytes.extend_from_slice(&bytes);

        let decoded = self
            .decoder
            .decode(&mut mut_bytes)
            .map_err(CodecError::DecodeFailure)?;
        Poll::Ready(Some(Ok(decoded)))
    }
}

impl<D> Stream for Subscriber<D>
where
    D: MessageDecoder + Send + Unpin,
{
    type Item = Result<D::Item>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Attempt to pop a message off of the current batch, if available.
        if let Some(bytes) = self.message_batch.as_mut().and_then(|b| b.pop()) {
            return self.decode_message(bytes);
        }

        // Otherwise, poll a new frame from the stream
        let frame = match futures::ready!(self.stream.poll_next_unpin(cx)) {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => return Poll::Ready(Some(Err(err))),
            None => return Poll::Ready(None),
        };

        match frame {
            // If the frame is a standard, unbatched message, then decode and return it
            // immediately.
            Frame::Message(mut payload) => {
                if let Some(decomp) = &self.decompression {
                    payload.message = decomp
                        .decompress(payload.message)
                        .map_err(CodecError::DecompressFailure)?;
                }

                self.decode_message(payload.message)
            }
            // If the frame is a batched message, then set the current batch and call `poll_next`
            // again to begin popping off messages.
            Frame::BatchMessage(mut payload) => {
                if let Some(decomp) = &self.decompression {
                    payload.message = decomp
                        .decompress(payload.message)
                        .map_err(CodecError::DecompressFailure)?;
                }

                let batch = decode_message_batch(payload.message);
                self.message_batch = Some(batch);
                self.poll_next(cx)
            }
            // Otherwise, do nothing.
            _ => Poll::Ready(None),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

impl<D> KeepAliveStream for Subscriber<D>
where
    D: MessageDecoder + Send + Unpin,
{
    type Headers = SubscriberPayload;

    fn reestablish_connection(connection: SharedConnection, headers: Self::Headers) -> AttemptFut {
        Box::pin(async move {
            let mut lock = connection.lock().await;
            lock.reconnect().await?;
            Self::open_stream(lock, headers).await
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
