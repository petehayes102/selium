use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::Codec;

pub struct Connection {
    inner: quinn::Connection,
    done_handshake: bool,
}

pub struct Stream<R, W> {
    read: FramedRead<R, Codec>,
    write: FramedWrite<W, Codec>,
    done_handshake: bool,
}

impl Connection {
    pub fn new(inner: quinn::Connection) -> Self {
        Self {
            inner,
            done_handshake: false,
        }
    }

    pub async fn accept<R, W>(&mut self, writer: W, reader: R) -> Stream<R, W>
    where
        R: AsyncRead,
        W: AsyncWrite,
    {
        if !self.done_handshake {
            self.handshake(&mut writer, &mut reader).await?;
            self.done_handshake = true;
        }
    }

    async fn handshake<R, W>(&self, writer: &mut W, reader: &mut R) -> Result<()>
    where
        R: AsyncRead,
        W: AsyncWrite,
    {
        let mut salutation = [0; 5];
        rx.read_exact(&mut salutation).await?;

        if &salutation[0..3] != b"SEL" {
            return Err(HandshakeError::InvalidSalutation);
        }

        let version = u16::from_le_bytes(salutation[3..5].try_into().unwrap());
        if version != VERSION {
            tx.write_all(b"BYE").await?;
            return Err(HandshakeError::UnsupportedVersion);
        }

        tx.write_all(b"HEY").await?;

        Ok(())
    }
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
