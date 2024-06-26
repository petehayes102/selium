use super::states::*;
use crate::connection::{ClientConnection, SharedConnection};
use crate::keep_alive::reqrep::KeepAlive;
use crate::keep_alive::AttemptFut;
use crate::streams::aliases::{Comp, Decomp};
use crate::streams::handle_reply;
use crate::traits::{KeepAliveStream, Open};
use crate::{Client, StreamBuilder};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{Future, SinkExt, StreamExt};
use selium_protocol::error_codes::UNKNOWN_ERROR;
use selium_protocol::{BiStream, Frame, MessagePayload, ReplierPayload, TopicName};
use selium_std::errors::{CodecError, Result, SeliumError};
use selium_std::traits::codec::{MessageDecoder, MessageEncoder};
use selium_std::traits::compression::{Compress, Decompress};
use std::fmt::Debug;
use std::{pin::Pin, sync::Arc};
use tokio::sync::MutexGuard;

impl StreamBuilder<ReplierWantsRequestDecoder> {
    /// Specifies the decoder a [Replier] uses for decoding
    /// incoming requests.
    ///
    /// A decoder can be any type implementing
    /// [MessageDecoder](crate::std::traits::codec::MessageDecoder).
    pub fn with_request_decoder<D>(self, decoder: D) -> StreamBuilder<ReplierWantsReplyEncoder<D>> {
        let next_state = ReplierWantsReplyEncoder::new(self.state, decoder);

        StreamBuilder {
            state: next_state,
            client: self.client,
        }
    }
}

impl<D> StreamBuilder<ReplierWantsReplyEncoder<D>> {
    /// Specifies the decompression implementation a [Replier] uses for
    /// decompressing incoming request payloads.
    ///
    /// A decompressor can be any type implementing
    /// [Decompress](crate::std::traits::compression::Decompress).
    pub fn with_request_decompression<T>(mut self, decomp: T) -> Self
    where
        T: Decompress + Send + Sync + 'static,
    {
        self.state.decompression = Some(Arc::new(decomp));
        self
    }

    /// Specifies the encoder a [Replier] uses for encoding outgoing replies.
    ///
    /// An encoder can be any type implementing
    /// [MessageEncoder](crate::std::traits::codec::MessageEncoder).
    pub fn with_reply_encoder<E>(self, encoder: E) -> StreamBuilder<ReplierWantsHandler<D, E>> {
        let next_state = ReplierWantsHandler::new(self.state, encoder);

        StreamBuilder {
            state: next_state,
            client: self.client,
        }
    }
}

impl<D, E> StreamBuilder<ReplierWantsHandler<D, E>> {
    /// Specifies the compression implementation a [Replier] uses for
    /// compressing outgoing replies.
    ///
    /// A compressor can be any type implementing [Compress](crate::std::traits::compression::Compress).
    pub fn with_reply_compression<T>(mut self, comp: T) -> Self
    where
        T: Compress + Send + Sync + 'static,
    {
        self.state.compression = Some(Arc::new(comp));
        self
    }

    /// Specifies the callback to invoke when handling incoming requests. The handler can be either
    /// closure returning an async block, or a pointer to an asynchronous function.
    ///
    /// The closure will receive the decoded request as a single argument.
    ///
    /// # Errors
    ///
    /// The handler function must return a [Result] to account for failures when processing
    /// requests.
    pub fn with_handler<Err, F, Fut>(self, handler: F) -> StreamBuilder<ReplierWantsOpen<D, E, F>>
    where
        D: MessageDecoder + Send + Unpin,
        E: MessageEncoder + Send + Unpin,
        F: FnMut(D::Item) -> Fut,
        Fut: Future<Output = std::result::Result<E::Item, Err>>,
    {
        let next_state = ReplierWantsOpen::new(self.state, handler);

        StreamBuilder {
            state: next_state,
            client: self.client,
        }
    }
}

#[async_trait]
impl<D, E, Err, F, Fut> Open for StreamBuilder<ReplierWantsOpen<D, E, F>>
where
    D: MessageDecoder + Send + Unpin,
    E: MessageEncoder + Send + Unpin,
    Err: Debug,
    F: FnMut(D::Item) -> Fut + Send + Unpin,
    Fut: Future<Output = std::result::Result<E::Item, Err>>,
{
    type Output = KeepAlive<Replier<E, D, F>>;

    async fn open(self) -> Result<Self::Output> {
        let topic = TopicName::try_from(self.state.endpoint.as_str())?;

        let headers = ReplierPayload { topic };

        let replier = Replier::spawn(
            self.client,
            headers,
            self.state.encoder,
            self.state.decoder,
            self.state.compression,
            self.state.decompression,
            self.state.handler,
        )
        .await?;

        Ok(replier)
    }
}

/// A Replier stream that binds to a topic, and listens for incoming requests from one or more
/// [Requestor](crate::streams::request_reply::Requestor) streams.
///
/// When a request is received by a Replier stream, a provided callback will be invoked to process the
/// request and return a response back to the [Requestor](crate::streams::request_reply::Requestor) stream.
///
/// When a Replier stream is spawned, it will bind to the specified topic. A consequence of this is
/// that only one active stream can bind to a namespace/topic combination at any given time. Trying
/// to bind to an already occupied topic will result in a runtime error.
pub struct Replier<E, D, F> {
    client: Client,
    stream: BiStream,
    headers: ReplierPayload,
    encoder: E,
    decoder: D,
    compression: Option<Comp>,
    decompression: Option<Decomp>,
    handler: Pin<Box<F>>,
}

impl<D, E, Err, F, Fut> Replier<E, D, F>
where
    D: MessageDecoder + Send + Unpin,
    E: MessageEncoder + Send + Unpin,
    Err: Debug,
    F: FnMut(D::Item) -> Fut + Send + Unpin,
    Fut: Future<Output = std::result::Result<E::Item, Err>>,
{
    async fn spawn(
        client: Client,
        headers: ReplierPayload,
        encoder: E,
        decoder: D,
        compression: Option<Comp>,
        decompression: Option<Decomp>,
        handler: Pin<Box<F>>,
    ) -> Result<KeepAlive<Self>> {
        let lock = client.connection.lock().await;
        let stream = Self::open_stream(lock, headers.clone()).await?;

        let replier = Self {
            client: client.clone(),
            stream,
            headers,
            encoder,
            decoder,
            compression,
            decompression,
            handler,
        };

        Ok(KeepAlive::new(replier, client.backoff_strategy))
    }

    async fn open_stream(
        lock: MutexGuard<'_, ClientConnection>,
        headers: ReplierPayload,
    ) -> Result<BiStream> {
        let mut stream = BiStream::try_from_connection(lock.conn()).await?;
        drop(lock);

        let frame = Frame::RegisterReplier(headers);
        stream.send(frame).await?;

        handle_reply(&mut stream).await?;
        Ok(stream)
    }

    fn decode_message(&mut self, mut bytes: Bytes) -> Result<D::Item> {
        if let Some(decomp) = self.decompression.as_ref() {
            bytes = decomp
                .decompress(bytes)
                .map_err(CodecError::DecompressFailure)?;
        }

        let mut mut_bytes = BytesMut::with_capacity(bytes.len());
        mut_bytes.extend_from_slice(&bytes);

        Ok(self
            .decoder
            .decode(&mut mut_bytes)
            .map_err(CodecError::DecodeFailure)?)
    }

    fn encode_message(&mut self, item: E::Item) -> Result<Bytes> {
        let mut encoded = self
            .encoder
            .encode(item)
            .map_err(CodecError::EncodeFailure)?;

        if let Some(comp) = self.compression.as_ref() {
            encoded = comp
                .compress(encoded)
                .map_err(CodecError::CompressFailure)?;
        }

        Ok(encoded)
    }

    async fn handle_request(&mut self, req_payload: MessagePayload) -> Result<()> {
        let decoded = self.decode_message(req_payload.message)?;
        let response = (self.handler)(decoded)
            .await
            .map_err(|e| SeliumError::RequestHandlerFailure(format!("{e:?}")))?;
        let encoded = self.encode_message(response)?;

        let res_payload = MessagePayload {
            headers: req_payload.headers,
            message: encoded,
        };

        let frame = Frame::Message(res_payload);
        self.stream.send(frame).await?;

        Ok(())
    }

    /// Prepares a [Replier] stream to begin processing incoming messages.
    /// This method will block the current task until the stream has been exhausted.
    pub async fn listen(&mut self) -> Result<()> {
        while let Some(frame) = self.stream.next().await {
            self.handle_frame(frame).await?;
        }

        Ok(())
    }

    async fn handle_frame(&mut self, frame: Result<Frame>) -> Result<()> {
        match frame {
            Ok(Frame::Message(req)) => Ok(self.handle_request(req).await?),
            Ok(Frame::Error(payload)) => match String::from_utf8(payload.message.to_vec()) {
                Ok(s) => Err(SeliumError::OpenStream(payload.code, s)),
                Err(_) => Err(SeliumError::OpenStream(
                    payload.code,
                    "Invalid UTF-8 error".into(),
                )),
            },
            Ok(_) => Err(SeliumError::OpenStream(
                UNKNOWN_ERROR,
                "Invalid frame returned from server".into(),
            )),
            Err(err) => Err(err),
        }
    }
}

impl<D, E, Err, F, Fut> KeepAliveStream for Replier<E, D, F>
where
    D: MessageDecoder + Send + Unpin,
    E: MessageEncoder + Send + Unpin,
    Err: Debug,
    F: FnMut(D::Item) -> Fut + Send + Unpin,
    Fut: Future<Output = std::result::Result<E::Item, Err>>,
{
    type Headers = ReplierPayload;

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
