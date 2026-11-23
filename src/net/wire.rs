//! Messages on a byte stream: a big-endian `u32` length, then MessagePack.
//! Used between devices and on the local control socket.

use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest message either side accepts.
pub const MAX_MESSAGE: usize = 16 << 20;

pub async fn send<T: Serialize>(out: &mut (impl AsyncWrite + Unpin), message: &T) -> Result<()> {
    let bytes = rmp_serde::to_vec_named(message).context("encoding a message")?;
    ensure!(
        bytes.len() <= MAX_MESSAGE,
        "a message of {} bytes is too large",
        bytes.len()
    );
    let len = u32::try_from(bytes.len()).context("a message too large")?;
    out.write_all(&len.to_be_bytes()).await?;
    out.write_all(&bytes).await?;
    Ok(())
}

/// The next message, or `None` when the stream ended cleanly between
/// messages.
pub async fn recv<T: DeserializeOwned>(input: &mut (impl AsyncRead + Unpin)) -> Result<Option<T>> {
    let mut len = [0; 4];

    match input.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("reading a message"),
    }

    let len = u32::from_be_bytes(len) as usize;
    ensure!(len <= MAX_MESSAGE, "a message of {len} bytes is too large");
    let mut bytes = vec![0; len];
    input
        .read_exact(&mut bytes)
        .await
        .context("reading a message")?;
    rmp_serde::from_slice(&bytes)
        .context("decoding a message")
        .map(Some)
}

/// The next message, failing when the stream ends.
pub async fn expect<T: DeserializeOwned>(input: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    recv(input).await?.context("the peer closed the stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        send(&mut a, &("hello", 42u32)).await.expect("sent");
        drop(a);

        let got: (String, u32) = expect(&mut b).await.expect("received");
        assert_eq!(got, ("hello".to_owned(), 42));
        assert!(
            recv::<(String, u32)>(&mut b)
                .await
                .expect("clean end")
                .is_none()
        );
    }
}
