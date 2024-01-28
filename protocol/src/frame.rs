use std::{
    fmt::{Display, Formatter},
    path::PathBuf,
};

use bincode::{Decode, Encode};

#[derive(Decode, Encode)]
pub enum Signal {
    CloudAuthFailed,
    InvalidTopicName,
    ReplierAlreadyBound,
    Shutdown,
    ShutdownInProgress,
    StreamClosedPrematurely,
    UnknownError,
}

#[derive(Copy, Clone, Debug, Decode, Encode, PartialEq)]
pub enum StreamType {
    Publisher,
    Subscriber,
    Requestor,
    Replier,
}

#[derive(Decode, Encode)]
pub enum Frame {
    Batch(Vec<u8>),
    Message(Vec<u8>),
    NewStream(StreamType, PathBuf),
    Reply(u32, Vec<u8>),
    Request(u32, Vec<u8>),
    ServerReply(u64, u32, Vec<u8>),
    ServerRequest(u64, u32, Vec<u8>),
    Signal(Signal), // Signals from server to client
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FrameType {
    Init, // used to initialize Codec before any frames have been received
    Batch,
    Message,
    NewStream,
    Reply,
    Request,
    ServerReply,
    ServerRequest,
    Signal,
}

impl Frame {
    pub(super) fn ty(&self) -> FrameType {
        match self {
            Frame::Batch(_) => FrameType::Batch,
            Frame::Message(_) => FrameType::Message,
            Frame::NewStream(_, _) => FrameType::NewStream,
            Frame::Reply(_, _) => FrameType::Reply,
            Frame::Request(_, _) => FrameType::Request,
            Frame::ServerReply(_, _, _) => FrameType::ServerReply,
            Frame::ServerRequest(_, _, _) => FrameType::ServerRequest,
            Frame::Signal(_) => FrameType::Signal,
        }
    }

    pub(super) fn size(&self) -> usize {
        match self {
            Frame::Batch(bytes) => bytes.len(),
            Frame::Message(bytes) => bytes.len(),
            Frame::NewStream(_, path) => 1 + path.capacity(),
            Frame::Reply(_, bytes) | Frame::Request(_, bytes) => 4 + bytes.len(),
            Frame::ServerReply(_, _, bytes) | Frame::ServerRequest(_, _, bytes) => {
                8 + 4 + bytes.len()
            }
            Frame::Signal(_) => 1,
        }
    }
}

impl Display for FrameType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init => write!(f, "Frame::Init"),
            Self::Batch => write!(f, "Frame::Batch"),
            Self::Message => write!(f, "Frame::Message"),
            Self::NewStream => write!(f, "Frame::NewStream"),
            Self::Reply => write!(f, "Frame::Reply"),
            Self::Request => write!(f, "Frame::Request"),
            Self::ServerReply => write!(f, "Frame::ServerReply"),
            Self::ServerRequest => write!(f, "Frame::ServerRequest"),
            Self::Signal => write!(f, "Frame::Signal"),
        }
    }
}

impl From<FrameType> for u8 {
    fn from(value: FrameType) -> Self {
        value as u8
    }
}
