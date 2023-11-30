use super::states::{SubscriberWantsDecoder, SubscriberWantsOpen};
use crate::connection::{ClientConnection, SharedConnection};
use crate::keep_alive::{AttemptFut, KeepAlive};
use crate::streams::aliases::Decomp;
use crate::traits::{KeepAliveStream, Open, Operations, Retain, TryIntoU64};
use crate::{Client, StreamBuilder};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, Stream, StreamExt};
use selium_protocol::utils::decode_message_batch;
use selium_protocol::{BiStream, Frame, SubscriberPayload};
use selium_std::errors::{CodecError, Result};
use selium_std::traits::codec::MessageDecoder;
use selium_std::traits::compression::Decompress;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::MutexGuard;

impl StreamBuilder<SubscriberWantsDecoder> {
    /// Specifies the decoder a [Subscriber](crate::Subscriber) uses for decoding messages
    /// received over the wire.
    ///
    /// A decoder can be any type implementing
    /// [MessageDecoder](crate::std::traits::codec::MessageDecoder).
    pub fn with_decoder<D, Item>(self, decoder: D) -> StreamBuilder<SubscriberWantsOpen<D, Item>> {
        let next_state = SubscriberWantsOpen::new(self.state, decoder);

        StreamBuilder {
            state: next_state,
            client: self.client,
        }
    }
}

impl<D, Item> StreamBuilder<SubscriberWantsOpen<D, Item>> {
    /// Specifies the decompression implementation a [Subscriber](crate::Subscriber) uses for
    /// decompressing messages received over the wire prior to decoding.
    ///
    /// A decompressor can be any type implementing
    /// [Decompress](crate::std::traits::compression::Decompress).
    pub fn with_decompression<T>(mut self, decomp: T) -> StreamBuilder<SubscriberWantsOpen<D, Item>>
    where
        T: Decompress + Send + Sync + 'static,
    {
        self.state.decompression = Some(Arc::new(decomp));
        self
    }
}

impl<D, Item> Retain for StreamBuilder<SubscriberWantsOpen<D, Item>> {
    fn retain<T: TryIntoU64>(mut self, policy: T) -> Result<Self> {
        self.state.common.retain(policy)?;
        Ok(self)
    }
}

impl<D, Item> Operations for StreamBuilder<SubscriberWantsOpen<D, Item>> {
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
impl<D, Item> Open for StreamBuilder<SubscriberWantsOpen<D, Item>>
where
    D: MessageDecoder<Item> + Send + Unpin,
    Item: Send + Unpin,
{
    type Output = KeepAlive<Subscriber<D, Item>, Item>;

    async fn open(self) -> Result<Self::Output> {
        let headers = SubscriberPayload {
            topic: self.state.common.topic,
            retention_policy: self.state.common.retention_policy,
            operations: self.state.common.operations,
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
pub struct Subscriber<D, Item> {
    client: Client,
    stream: BiStream,
    headers: SubscriberPayload,
    decoder: D,
    decompression: Option<Decomp>,
    message_batch: Option<Vec<Bytes>>,
    _marker: PhantomData<Item>,
}

impl<D, Item> Subscriber<D, Item>
where
    D: MessageDecoder<Item> + Send + Unpin,
    Item: Unpin + Send,
{
    async fn spawn(
        client: Client,
        headers: SubscriberPayload,
        decoder: D,
        decompression: Option<Decomp>,
    ) -> Result<KeepAlive<Self, Item>> {
        let lock = client.connection.lock().await;
        let mut stream = Self::open_stream(lock, headers.clone()).await?;
        stream.finish().await?;

        let subscriber = Self {
            client: client.clone(),
            stream,
            headers,
            decoder,
            message_batch: None,
            decompression,
            _marker: PhantomData,
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

        Ok(stream)
    }

    fn decode_message(&mut self, bytes: Bytes) -> Poll<Option<Result<Item>>> {
        let mut mut_bytes = BytesMut::with_capacity(bytes.len());
        mut_bytes.extend_from_slice(&bytes);

        let decoded = self
            .decoder
            .decode(&mut mut_bytes)
            .map_err(CodecError::DecodeFailure)?;
        Poll::Ready(Some(Ok(decoded)))
    }
}

impl<D, Item> Stream for Subscriber<D, Item>
where
    D: MessageDecoder<Item> + Send + Unpin,
    Item: Send + Unpin,
{
    type Item = Result<Item>;

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
            Frame::BatchMessage(mut bytes) => {
                if let Some(decomp) = &self.decompression {
                    bytes = decomp
                        .decompress(bytes)
                        .map_err(CodecError::DecompressFailure)?;
                }

                let batch = decode_message_batch(bytes);
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

impl<D, Item> KeepAliveStream for Subscriber<D, Item>
where
    D: MessageDecoder<Item> + Send + Unpin,
    Item: Unpin + Send,
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