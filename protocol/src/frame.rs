use crate::Operation;
use bytes::{BufMut, Bytes, BytesMut};
use selium_std::errors::{ProtocolError, Result, SeliumError};
use serde::{Deserialize, Serialize};

const REGISTER_PUBLISHER: u8 = 0x0;
const REGISTER_SUBSCRIBER: u8 = 0x1;
const MESSAGE: u8 = 0x2;
const BATCH_MESSAGE: u8 = 0x3;

#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    RegisterPublisher(PublisherPayload),
    RegisterSubscriber(SubscriberPayload),
    Message(Bytes),
    BatchMessage(Bytes),
}

impl Frame {
    pub fn get_length(&self) -> Result<u64> {
        let length = match self {
            Self::RegisterPublisher(payload) => {
                bincode::serialized_size(payload).map_err(ProtocolError::SerdeError)?
            }
            Self::RegisterSubscriber(payload) => {
                bincode::serialized_size(payload).map_err(ProtocolError::SerdeError)?
            }
            Self::Message(bytes) => bytes.len() as u64,
            Self::BatchMessage(bytes) => bytes.len() as u64,
        };

        Ok(length)
    }

    pub fn get_type(&self) -> u8 {
        match self {
            Self::RegisterPublisher(_) => REGISTER_PUBLISHER,
            Self::RegisterSubscriber(_) => REGISTER_SUBSCRIBER,
            Self::Message(_) => MESSAGE,
            Self::BatchMessage(_) => BATCH_MESSAGE,
        }
    }

    pub fn get_topic(&self) -> Option<&str> {
        match self {
            Self::RegisterPublisher(p) => Some(&p.topic),
            Self::RegisterSubscriber(s) => Some(&s.topic),
            Self::Message(_) => None,
            Self::BatchMessage(_) => None,
        }
    }

    pub fn write_to_bytes(self, dst: &mut BytesMut) -> Result<()> {
        match self {
            Frame::RegisterPublisher(payload) => bincode::serialize_into(dst.writer(), &payload)
                .map_err(ProtocolError::SerdeError)?,
            Frame::RegisterSubscriber(payload) => bincode::serialize_into(dst.writer(), &payload)
                .map_err(ProtocolError::SerdeError)?,
            Frame::Message(bytes) => dst.extend_from_slice(&bytes),
            Frame::BatchMessage(bytes) => dst.extend_from_slice(&bytes),
        }

        Ok(())
    }
}

impl TryFrom<(u8, BytesMut)> for Frame {
    type Error = SeliumError;

    fn try_from((message_type, bytes): (u8, BytesMut)) -> Result<Self, Self::Error> {
        let frame = match message_type {
            REGISTER_PUBLISHER => Frame::RegisterPublisher(
                bincode::deserialize(&bytes).map_err(ProtocolError::SerdeError)?,
            ),
            REGISTER_SUBSCRIBER => Frame::RegisterSubscriber(
                bincode::deserialize(&bytes).map_err(ProtocolError::SerdeError)?,
            ),
            MESSAGE => Frame::Message(bytes.into()),
            BATCH_MESSAGE => Frame::BatchMessage(bytes.into()),
            _type => return Err(ProtocolError::UnknownMessageType(_type))?,
        };

        Ok(frame)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublisherPayload {
    pub topic: String,
    pub retention_policy: u64,
    pub operations: Vec<Operation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubscriberPayload {
    pub topic: String,
    pub retention_policy: u64,
    pub operations: Vec<Operation>,
}
