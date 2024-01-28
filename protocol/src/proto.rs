use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::Codec;

pub struct Proto<R, W> {
    read: FramedRead<R, Codec>,
    write: FramedWrite<W, Codec>,
}

impl<R, W> Proto<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    pub fn new(reader: R, writer: W) -> Proto<R, W> {
        Self {
            read: FramedRead::new(),
        }
    }
}
