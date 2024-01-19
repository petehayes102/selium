use crate::TopicName;
use bytes::{BufMut, Bytes, BytesMut};
use selium_std::errors::{ProtocolError, Result, SeliumError};
use std::collections::HashMap;

const REGISTER_PUBLISHER: u8 = 0x0;
const REGISTER_SUBSCRIBER: u8 = 0x1;
const REGISTER_REPLIER: u8 = 0x2;
const REGISTER_REQUESTOR: u8 = 0x3;
const MESSAGE: u8 = 0x4;
const BATCH_MESSAGE: u8 = 0x5;
const ERROR: u8 = 0x6;
const OK: u8 = 0x7;

#[derive(Debug, PartialEq)]
pub enum ConnectFrame {
    // Topic name, retention policy
    RegisterPublisher(TopicName, u64),
    // Topic name, retention policy
    RegisterSubscriber(TopicName, u64),
    RegisterReplier(TopicName),
    RegisterRequestor(TopicName),
}

#[derive(Debug, PartialEq)]
pub enum RequestFrame {
    // Headers, Message
    Message(Option<HashMap<String, String>>, Bytes),
    BatchMessage(Bytes),
}

pub type ReplyFrame = Result<(), (u32, Bytes)>;

// impl Frame {
//     pub fn get_length(&self) -> Result<u64> {
//         Ok(match self {
//             Self::RegisterPublisher(payload) => {
//                 bincode::serialized_size(payload).map_err(ProtocolError::SerdeError)?
//             }
//             Self::RegisterSubscriber(payload) => {
//                 bincode::serialized_size(payload).map_err(ProtocolError::SerdeError)?
//             }
//             Self::RegisterReplier(payload) => {
//                 bincode::serialized_size(payload).map_err(ProtocolError::SerdeError)?
//             }
//             Self::RegisterRequestor(payload) => {
//                 bincode::serialized_size(payload).map_err(ProtocolError::SerdeError)?
//             }
//             Self::Message(payload) => {
//                 bincode::serialized_size(payload).map_err(ProtocolError::SerdeError)?
//             }
//             Self::BatchMessage(bytes) => bytes.len() as u64,
//             Self::Error(bytes) => bytes.len() as u64,
//             Self::Ok => 0,
//         })
//     }

//     pub fn get_type(&self) -> u8 {
//         match self {
//             Self::RegisterPublisher(_) => REGISTER_PUBLISHER,
//             Self::RegisterSubscriber(_) => REGISTER_SUBSCRIBER,
//             Self::RegisterReplier(_) => REGISTER_REPLIER,
//             Self::RegisterRequestor(_) => REGISTER_REQUESTOR,
//             Self::Message(_) => MESSAGE,
//             Self::BatchMessage(_) => BATCH_MESSAGE,
//             Self::Error(_) => ERROR,
//             Self::Ok => OK,
//         }
//     }

//     pub fn get_topic(&self) -> Option<&TopicName> {
//         match self {
//             Self::RegisterPublisher(p) => Some(&p.topic),
//             Self::RegisterSubscriber(s) => Some(&s.topic),
//             Self::RegisterReplier(s) => Some(&s.topic),
//             Self::RegisterRequestor(c) => Some(&c.topic),
//             Self::Message(_) => None,
//             Self::BatchMessage(_) => None,
//             Self::Error(_) => None,
//             Self::Ok => None,
//         }
//     }

//     pub fn write_to_bytes(self, dst: &mut BytesMut) -> Result<()> {
//         match self {
//             Frame::RegisterPublisher(payload) => bincode::serialize_into(dst.writer(), &payload)
//                 .map_err(ProtocolError::SerdeError)?,
//             Frame::RegisterSubscriber(payload) => bincode::serialize_into(dst.writer(), &payload)
//                 .map_err(ProtocolError::SerdeError)?,
//             Frame::RegisterReplier(payload) => bincode::serialize_into(dst.writer(), &payload)
//                 .map_err(ProtocolError::SerdeError)?,
//             Frame::RegisterRequestor(payload) => bincode::serialize_into(dst.writer(), &payload)
//                 .map_err(ProtocolError::SerdeError)?,
//             Frame::Message(payload) => bincode::serialize_into(dst.writer(), &payload)
//                 .map_err(ProtocolError::SerdeError)?,
//             Frame::BatchMessage(bytes) => dst.extend_from_slice(&bytes),
//             Frame::Error(bytes) => dst.extend_from_slice(&bytes),
//             Frame::Ok => (),
//         }

//         Ok(())
//     }

//     pub fn unwrap_message(self) -> MessagePayload {
//         match self {
//             Self::Message(p) => p,
//             _ => panic!("Attempted to unwrap non-Message Frame variant"),
//         }
//     }
// }

// impl TryFrom<(u8, BytesMut)> for Frame {
//     type Error = SeliumError;

//     fn try_from(
//         (message_type, bytes): (u8, BytesMut),
//     ) -> Result<Self, <Frame as TryFrom<(u8, BytesMut)>>::Error> {
//         let frame = match message_type {
//             REGISTER_PUBLISHER => Frame::RegisterPublisher(
//                 bincode::deserialize(&bytes).map_err(ProtocolError::SerdeError)?,
//             ),
//             REGISTER_SUBSCRIBER => Frame::RegisterSubscriber(
//                 bincode::deserialize(&bytes).map_err(ProtocolError::SerdeError)?,
//             ),
//             REGISTER_REPLIER => Frame::RegisterReplier(
//                 bincode::deserialize(&bytes).map_err(ProtocolError::SerdeError)?,
//             ),
//             REGISTER_REQUESTOR => Frame::RegisterRequestor(
//                 bincode::deserialize(&bytes).map_err(ProtocolError::SerdeError)?,
//             ),
//             MESSAGE => {
//                 Frame::Message(bincode::deserialize(&bytes).map_err(ProtocolError::SerdeError)?)
//             }
//             BATCH_MESSAGE => Frame::BatchMessage(bytes.into()),
//             ERROR => Frame::Error(bytes.into()),
//             OK => Frame::Ok,
//             _type => return Err(ProtocolError::UnknownMessageType(_type))?,
//         };

//         Ok(frame)
//     }
// }
