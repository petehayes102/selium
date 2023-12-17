use std::{pin::Pin, time::Duration};

use anyhow::{anyhow, Context, Result as AnyhowResult};
use bytes::BytesMut;
use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    future, Sink, SinkExt, Stream, StreamExt,
};
use quinn::Connection;
use selium_protocol::{Frame, MessagePayload, TopicName};
use selium_proxy::{AdminRequest, AdminResponse};
use selium_std::{
    codecs::BincodeCodec,
    errors::{CodecError, Result, SeliumError},
    traits::codec::{MessageDecoder, MessageEncoder},
};
use tokio::time::timeout;

use crate::{
    quic::get_pubkey_from_connection,
    server::SharedTopics,
    topic::{reqrep, Socket},
};

#[cfg(debug_assertions)]
const PROXY_PUBKEY: &[u8; 469] = include_bytes!("../../proxy.debug.der");
#[cfg(not(debug_assertions))]
const PROXY_PUBKEY: &[u8; 470] = include_bytes!("../../proxy.prod.der");

// XXX This is horrendously inefficient! Caching is needed.
pub async fn do_cloud_auth(
    connection: &Connection,
    name: &TopicName,
    topics: &SharedTopics,
) -> AnyhowResult<()> {
    let pub_key = get_pubkey_from_connection(&connection)?;

    // If this is the proxy, don't do auth
    if pub_key.as_bytes() == PROXY_PUBKEY {
        return Ok(());
    }

    let mut ts = topics.lock().await;

    let proxy_namespace = TopicName::_create_unchecked("selium", "proxy");

    let namespace = name.namespace();

    if ts.contains_key(&proxy_namespace) {
        let ((si, st), (mut tx, rx)) = channel_pair();

        let topic_tx = ts.get_mut(&proxy_namespace).unwrap();
        topic_tx
            .send(Socket::Reqrep(reqrep::Socket::Client((
                Box::pin(si.sink_map_err(|_| SeliumError::RequestFailed)),
                Box::pin(st),
            ))))
            .await
            .context("Failed to add Requestor to proxy topic")?;

        tx.send(AdminRequest::GetNamespace(pub_key)).await?;
        let result = timeout(Duration::from_secs(5), rx.into_future()).await;
        match result {
            Ok((Some(Ok(AdminResponse::GetNamespaceResponse(ns))), _)) if ns == namespace => Ok(()),
            Ok((Some(Ok(AdminResponse::GetNamespaceResponse(_))), _)) => {
                Err(anyhow!("Access denied"))
            }
            Ok((Some(Ok(_)), _)) => Err(anyhow!("Invalid response from proxy")),
            Ok((Some(Err(e)), _)) => Err(e.into()),
            _ => Err(anyhow!("No response from proxy")),
        }
    } else {
        Err(anyhow!("Waiting for proxy to connect - please retry"))
    }
}

fn channel_pair() -> (
    (UnboundedSender<Frame>, UnboundedReceiver<Result<Frame>>),
    (
        Pin<Box<dyn Sink<AdminRequest, Error = SeliumError> + Send>>,
        Pin<Box<dyn Stream<Item = Result<AdminResponse>> + Send>>,
    ),
) {
    let (si, rx) = unbounded();
    let (tx, st) = unbounded();

    let bincode = BincodeCodec::default();
    let tx = tx
        .sink_map_err(|_| SeliumError::RequestFailed)
        .with(move |item| match bincode.encode(item) {
            Ok(msg) => future::ok(Ok(Frame::Message(MessagePayload {
                headers: None,
                message: msg,
            }))),
            Err(e) => future::err(SeliumError::Codec(CodecError::EncodeFailure(e))),
        });

    let bincode = BincodeCodec::default();
    let rx = rx.map(move |frame| match frame {
        Frame::Message(payload) => {
            let mut bytes = BytesMut::new();
            bytes.extend(payload.message);
            match bincode.decode(&mut bytes) {
                Ok(item) => Ok(item),
                Err(e) => Err(SeliumError::Codec(CodecError::DecodeFailure(e))),
            }
        }
        _ => Err(SeliumError::RequestFailed),
    });

    ((si, st), (Box::pin(tx), Box::pin(rx)))
}