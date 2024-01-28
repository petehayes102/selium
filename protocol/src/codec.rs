use std::{io, path::PathBuf};

use crate::frame::{Frame, FrameType, StreamType};
use bincode::{
    config::Configuration,
    error::{DecodeError, EncodeError},
};
use bytes::BytesMut;
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};

const MAX_FRAME_SIZE: usize = 1024 * 1024; // 1 mebibyte
const SENDING: bool = true;
const RECEIVING: bool = false;

pub struct Codec {
    bincode_conf: Configuration,
    last_frame: FrameType,
    max_frame_size: usize,
    stream_type: Option<StreamType>,
    path: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("could not decode Frame: {0:?}")]
    Decode(#[from] DecodeError),
    #[error("could not encode Frame: {0:?}")]
    Encode(#[from] EncodeError),
    #[error("Frame size ({0} bytes) is greater than maximum allowed size ({1} bytes).")]
    FrameTooLarge(usize, usize),
    #[error("Unexpected Frame {0} received after {1}")]
    FrameOutOfOrder(FrameType, FrameType),
    #[error("error during read/write operation: {0:?}")]
    Io(#[from] io::Error),
}

impl Codec {
    pub fn get_path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }
}

impl Codec {
    fn is_publisher(&self) -> bool {
        self.stream_type == Some(StreamType::Publisher)
    }

    fn is_subscriber(&self) -> bool {
        self.stream_type == Some(StreamType::Subscriber)
    }

    fn is_replier(&self) -> bool {
        self.stream_type == Some(StreamType::Replier)
    }

    fn is_requestor(&self) -> bool {
        self.stream_type == Some(StreamType::Requestor)
    }

    fn is_valid_newstream(&self, sending: bool) -> bool {
        sending && self.last_frame == FrameType::Init
    }

    fn is_valid_pubsub(&self, sending: bool) -> bool {
        ((self.is_publisher() && sending) || (self.is_subscriber() && !sending))
            && [
                FrameType::NewStream,
                FrameType::Message,
                FrameType::Batch,
                FrameType::Signal,
            ]
            .contains(&self.last_frame)
    }

    fn is_valid_reply(&self, sending: bool) -> bool {
        self.is_requestor()
            && !sending
            && [FrameType::Request, FrameType::Reply, FrameType::Signal].contains(&self.last_frame)
    }

    fn is_valid_request(&self, sending: bool) -> bool {
        self.is_requestor()
            && sending
            && [
                FrameType::NewStream,
                FrameType::Request,
                FrameType::Reply,
                FrameType::Signal,
            ]
            .contains(&self.last_frame)
    }

    fn is_valid_server_reply(&self, sending: bool) -> bool {
        self.is_replier()
            && sending
            && [
                FrameType::ServerRequest,
                FrameType::ServerReply,
                FrameType::Signal,
            ]
            .contains(&self.last_frame)
    }

    fn is_valid_server_request(&self, sending: bool) -> bool {
        self.is_replier()
            && !sending
            && [
                FrameType::NewStream,
                FrameType::ServerRequest,
                FrameType::ServerReply,
                FrameType::Signal,
            ]
            .contains(&self.last_frame)
    }

    fn state(&mut self, item: &Frame, sending: bool) -> Result<(), CodecError> {
        match item {
            Frame::Batch(_) | Frame::Message(_) if self.is_valid_pubsub(sending) => (),
            Frame::NewStream(ty, path) if self.is_valid_newstream(sending) => {
                self.stream_type = Some(*ty);
                self.path = Some(path.to_owned());
            }
            Frame::Reply(_, _) if self.is_valid_reply(sending) => (),
            Frame::Request(_, _) if self.is_valid_request(sending) => (),
            Frame::ServerReply(_, _, _) if self.is_valid_server_reply(sending) => (),
            Frame::ServerRequest(_, _, _) if self.is_valid_server_request(sending) => (),
            Frame::Signal(_) if !sending => (),
            _ => return Err(CodecError::FrameOutOfOrder(item.ty(), self.last_frame)),
        }

        self.last_frame = item.ty();

        Ok(())
    }
}

impl Default for Codec {
    fn default() -> Self {
        Codec {
            bincode_conf: bincode::config::standard(),
            last_frame: FrameType::Init,
            max_frame_size: MAX_FRAME_SIZE,
            stream_type: None,
            path: None,
        }
    }
}

impl Encoder<Frame> for Codec {
    type Error = CodecError;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        self.state(&item, SENDING)?;

        let size = item.size();
        if size > self.max_frame_size {
            return Err(CodecError::FrameTooLarge(size, self.max_frame_size));
        }

        bincode::encode_into_slice(&item, dst, self.bincode_conf)?;

        Ok(())
    }
}

impl Decoder for Codec {
    type Item = Frame;
    type Error = CodecError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, Self::Error> {
        let frame: Frame = match bincode::decode_from_slice(buf, self.bincode_conf) {
            Ok((frame, _)) => frame,
            Err(DecodeError::UnexpectedEnd { additional }) => {
                buf.reserve(additional);
                return Ok(None);
            }
            Err(e) => {
                return Err(e.into());
            }
        };

        self.state(&frame, RECEIVING)?;

        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::frame::Signal;

    use super::*;

    fn check_state(frames: &[(Frame, bool)]) {
        let mut codec = Codec::default();
        frames
            .iter()
            .for_each(|(frame, sending)| codec.state(frame, *sending).unwrap());
    }

    #[test]
    fn test_newstream_state() {
        let mut codec = Codec::default();
        codec
            .state(
                &Frame::NewStream(StreamType::Publisher, "/moo/cow".into()),
                SENDING,
            )
            .unwrap();

        assert_eq!(codec.stream_type, Some(StreamType::Publisher));
        assert_eq!(
            codec.get_path(),
            Some(&PathBuf::from_str("/moo/cow").unwrap())
        );
    }

    #[test]
    fn test_pub_ok() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Publisher, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Message(Vec::new()), SENDING),
            (Frame::Message(Vec::new()), SENDING),
            (Frame::Batch(Vec::new()), SENDING),
            (Frame::Signal(Signal::UnknownError), RECEIVING),
            (Frame::Message(Vec::new()), SENDING),
        ]);
    }

    #[test]
    fn test_sub_ok() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Subscriber, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Signal(Signal::UnknownError), RECEIVING),
            (Frame::Message(Vec::new()), RECEIVING),
            (Frame::Message(Vec::new()), RECEIVING),
            (Frame::Batch(Vec::new()), RECEIVING),
            (Frame::Message(Vec::new()), RECEIVING),
        ]);
    }

    #[test]
    fn test_req_ok() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Requestor, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Request(0, Vec::new()), SENDING),
            (Frame::Reply(0, Vec::new()), RECEIVING),
            (Frame::Request(1, Vec::new()), SENDING),
            (Frame::Request(2, Vec::new()), SENDING),
            (Frame::Reply(2, Vec::new()), RECEIVING),
            (Frame::Reply(1, Vec::new()), RECEIVING),
            (Frame::Signal(Signal::UnknownError), RECEIVING),
        ]);
    }

    #[test]
    fn test_rep_ok() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Replier, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::ServerRequest(12, 0, Vec::new()), RECEIVING),
            (Frame::Signal(Signal::UnknownError), RECEIVING),
            (Frame::ServerRequest(16, 1, Vec::new()), RECEIVING),
            (Frame::ServerReply(16, 1, Vec::new()), SENDING),
            (Frame::ServerReply(12, 0, Vec::new()), SENDING),
            (Frame::ServerRequest(1056, 2, Vec::new()), RECEIVING),
            (Frame::ServerReply(1056, 2, Vec::new()), SENDING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_missing_newstream() {
        check_state(&[
            (Frame::ServerRequest(12, 0, Vec::new()), RECEIVING),
            (Frame::ServerRequest(16, 1, Vec::new()), RECEIVING),
            (Frame::ServerReply(16, 1, Vec::new()), SENDING),
            (Frame::ServerReply(12, 0, Vec::new()), SENDING),
            (Frame::ServerRequest(1056, 2, Vec::new()), RECEIVING),
            (Frame::ServerReply(1056, 2, Vec::new()), SENDING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_recv_newstream() {
        check_state(&[(
            Frame::NewStream(StreamType::Requestor, "/moo/cow".into()),
            RECEIVING,
        )]);
    }

    #[test]
    #[should_panic]
    fn test_rep_using_req_frames() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Replier, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Request(0, Vec::new()), RECEIVING),
            (Frame::Reply(0, Vec::new()), SENDING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_rep_reply_before_request() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Replier, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::ServerReply(0, 0, Vec::new()), SENDING),
            (Frame::ServerRequest(0, 0, Vec::new()), RECEIVING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_rep_receive_reply() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Replier, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::ServerReply(0, 0, Vec::new()), RECEIVING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_req_receive_request() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Requestor, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Request(0, Vec::new()), RECEIVING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_req_receive_server_reply() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Requestor, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::ServerReply(0, 0, Vec::new()), RECEIVING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_pub_recv_message() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Publisher, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Message(Vec::new()), RECEIVING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_sub_send_message() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Subscriber, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Message(Vec::new()), SENDING),
        ]);
    }

    #[test]
    #[should_panic]
    fn test_send_signal() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Subscriber, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Signal(Signal::UnknownError), SENDING),
        ]);
    }

    #[test]
    fn test_recv_signal() {
        check_state(&[
            (
                Frame::NewStream(StreamType::Subscriber, "/moo/cow".into()),
                SENDING,
            ),
            (Frame::Signal(Signal::UnknownError), RECEIVING),
        ]);
    }
}
