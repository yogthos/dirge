//! Content-Length framing for DAP.
//!
//! Ported from `src/lsp/jsonrpc.rs` — identical wire format used by DAP.
//! DAP, like LSP, uses HTTP-style framing on top of stdio:
//! ```text
//! Content-Length: <N>\r\n
//! \r\n
//! <body of N bytes>
//! ```

use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CONTENT_LENGTH_PREFIX: &str = "Content-Length:";

/// Maximum body length we'll accept. Prevents a malformed Content-Length value
/// from forcing us to allocate an unbounded buffer.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Write a single framed message body. Prepends `Content-Length` header.
pub async fn encode_frame<W>(writer: &mut W, body: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one framed message body. Errors on EOF mid-frame, missing/malformed
/// `Content-Length`, or a body length above [`MAX_BODY_BYTES`].
pub async fn decode_frame<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stream closed before complete frame header",
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix(CONTENT_LENGTH_PREFIX) {
            let value = rest.trim();
            let parsed: usize = value.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("malformed Content-Length value: {value:?}"),
                )
            })?;
            if parsed > MAX_BODY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("body length {parsed} exceeds cap {MAX_BODY_BYTES}"),
                ));
            }
            content_length = Some(parsed);
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "frame header missing Content-Length",
        )
    })?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    async fn decode(bytes: &[u8]) -> io::Result<Vec<u8>> {
        let mut reader = BufReader::new(bytes);
        decode_frame(&mut reader).await
    }

    #[tokio::test]
    async fn encode_decode_round_trip() {
        let mut buf = Vec::new();
        encode_frame(&mut buf, br#"{"seq":1}"#).await.unwrap();
        assert!(buf.starts_with(b"Content-Length: 9\r\n\r\n"));
        assert_eq!(decode(&buf).await.unwrap(), br#"{"seq":1}"#);
    }

    #[tokio::test]
    async fn decodes_two_frames_in_sequence() {
        let mut buf = Vec::new();
        encode_frame(&mut buf, b"AA").await.unwrap();
        encode_frame(&mut buf, b"BBB").await.unwrap();
        let mut reader = BufReader::new(&buf[..]);
        assert_eq!(decode_frame(&mut reader).await.unwrap(), b"AA");
        assert_eq!(decode_frame(&mut reader).await.unwrap(), b"BBB");
    }

    #[tokio::test]
    async fn extra_headers_are_ignored() {
        let raw = b"X-Foo: bar\r\nContent-Length: 2\r\n\r\nhi";
        assert_eq!(decode(raw).await.unwrap(), b"hi");
    }

    #[tokio::test]
    async fn missing_content_length_errors() {
        let err = decode(b"X-Foo: bar\r\n\r\n").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn malformed_content_length_errors() {
        let err = decode(b"Content-Length: abc\r\n\r\n").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn oversized_content_length_rejected_without_allocating() {
        let raw = format!("Content-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        let err = decode(raw.as_bytes()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn eof_mid_body_errors() {
        // Header claims 10 bytes; only 3 follow.
        let err = decode(b"Content-Length: 10\r\n\r\nabc").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn eof_mid_header_errors() {
        let err = decode(b"Content-Length: 5").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
