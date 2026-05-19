//! JSON-RPC framing for LSP.
//!
//! LSP uses HTTP-style framing on top of stdio:
//! ```text
//! Content-Length: <N>\r\n
//! \r\n
//! <body of N bytes>
//! ```
//! Content-Type is allowed by the spec but ignored — every modern server
//! sends UTF-8 JSON. Multiple frames can follow back-to-back on the same
//! stream, so the reader operates on a buffered source.

use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CONTENT_LENGTH_PREFIX: &str = "Content-Length:";

/// Maximum body length we'll accept. Prevents a malformed Content-Length value
/// from forcing us to allocate an unbounded buffer. LSP messages in the wild
/// are KB-scale; 16 MB is more than enough headroom.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Write a single LSP message frame to `writer`. The caller passes the raw
/// JSON body as bytes; framing prepends `Content-Length` + the blank line.
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

/// Read one LSP message frame from `reader`. Returns the raw body bytes.
/// Errors on EOF mid-frame, missing/malformed `Content-Length`, or a body
/// length above [`MAX_BODY_BYTES`].
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
            // End of headers.
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
        // Other headers (e.g. Content-Type) are ignored per the LSP spec.
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

    async fn roundtrip(body: &[u8]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        encode_frame(&mut buf, body).await.unwrap();
        let mut reader = BufReader::new(buf.as_slice());
        decode_frame(&mut reader).await.unwrap()
    }

    #[tokio::test]
    async fn roundtrip_simple_json_body() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        assert_eq!(roundtrip(body).await, body);
    }

    #[tokio::test]
    async fn roundtrip_unicode_body_preserves_byte_count() {
        // Multi-byte chars: byte length ≠ char count. Content-Length is bytes
        // per the LSP spec — this test pins that down.
        let body = "{\"text\":\"hello 🦀 rust\"}".as_bytes();
        assert_eq!(roundtrip(body).await, body);
    }

    #[tokio::test]
    async fn roundtrip_empty_body() {
        // Content-Length: 0 is valid (rare but legal). Verify we don't trip
        // over the zero-byte read.
        assert_eq!(roundtrip(b"").await, b"");
    }

    #[tokio::test]
    async fn encode_format_is_content_length_blank_body() {
        let mut buf: Vec<u8> = Vec::new();
        encode_frame(&mut buf, b"hello").await.unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "Content-Length: 5\r\n\r\nhello");
    }

    #[tokio::test]
    async fn decode_multiple_frames_from_one_reader() {
        let mut buf: Vec<u8> = Vec::new();
        encode_frame(&mut buf, b"first").await.unwrap();
        encode_frame(&mut buf, b"second").await.unwrap();
        encode_frame(&mut buf, b"third").await.unwrap();

        let mut reader = BufReader::new(buf.as_slice());
        assert_eq!(decode_frame(&mut reader).await.unwrap(), b"first");
        assert_eq!(decode_frame(&mut reader).await.unwrap(), b"second");
        assert_eq!(decode_frame(&mut reader).await.unwrap(), b"third");
    }

    #[tokio::test]
    async fn decode_skips_unknown_headers_before_blank_line() {
        let frame = "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
                     Content-Length: 5\r\n\
                     \r\n\
                     hello";
        let mut reader = BufReader::new(frame.as_bytes());
        let body = decode_frame(&mut reader).await.unwrap();
        assert_eq!(body, b"hello");
    }

    // Regression: tools/in-house frame encoders sometimes use \n line endings
    // instead of \r\n. Be lenient on input (\r is trimmed regardless) so we
    // don't break against non-conforming servers.
    #[tokio::test]
    async fn decode_tolerates_lone_newline_line_endings() {
        let frame = "Content-Length: 5\n\nhello";
        let mut reader = BufReader::new(frame.as_bytes());
        let body = decode_frame(&mut reader).await.unwrap();
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn decode_errors_on_missing_content_length() {
        let frame = "Content-Type: foo\r\n\r\nbody";
        let mut reader = BufReader::new(frame.as_bytes());
        let err = decode_frame(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("Content-Length"));
    }

    #[tokio::test]
    async fn decode_errors_on_malformed_content_length() {
        let frame = "Content-Length: not-a-number\r\n\r\n";
        let mut reader = BufReader::new(frame.as_bytes());
        let err = decode_frame(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("malformed"));
    }

    // Regression: a malicious or buggy server claiming a huge Content-Length
    // must not coerce us into allocating an unbounded buffer.
    #[tokio::test]
    async fn decode_errors_when_content_length_exceeds_cap() {
        let huge = MAX_BODY_BYTES + 1;
        let frame = format!("Content-Length: {huge}\r\n\r\n");
        let mut reader = BufReader::new(frame.as_bytes());
        let err = decode_frame(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds cap"));
    }

    #[tokio::test]
    async fn decode_errors_on_eof_mid_header() {
        let frame = "Content-Length: 5\r\n"; // no blank line, no body
        let mut reader = BufReader::new(frame.as_bytes());
        let err = decode_frame(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn decode_errors_on_eof_mid_body() {
        // Header claims 10 bytes, body has 3.
        let frame = "Content-Length: 10\r\n\r\nabc";
        let mut reader = BufReader::new(frame.as_bytes());
        let err = decode_frame(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    // Regression: a streaming reader may not produce a full frame in a single
    // poll. Verify the decoder reads incrementally without dropping bytes.
    #[tokio::test]
    async fn decode_handles_byte_at_a_time_reader() {
        let mut buf: Vec<u8> = Vec::new();
        encode_frame(&mut buf, b"hello").await.unwrap();

        // Wrap in a duplex pipe and feed byte-by-byte.
        let (client_read, mut server_write) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            for byte in buf {
                server_write.write_all(&[byte]).await.unwrap();
                // Yield to give the reader a chance to make partial progress.
                tokio::task::yield_now().await;
            }
            drop(server_write);
        });

        let mut reader = BufReader::new(client_read);
        let body = decode_frame(&mut reader).await.unwrap();
        writer.await.unwrap();
        assert_eq!(body, b"hello");
    }

    // Concurrency check: encode + decode in parallel over a duplex pipe.
    // Models the producer/consumer pattern used by the actual client task.
    #[tokio::test]
    async fn encode_decode_through_duplex_pipe() {
        let (mut client_side, mut server_side) = tokio::io::duplex(1024);

        let writer = tokio::spawn(async move {
            for msg in [b"alpha".as_slice(), b"beta", b"gamma"] {
                encode_frame(&mut server_side, msg).await.unwrap();
            }
            drop(server_side);
        });

        let mut reader = BufReader::new(&mut client_side);
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(decode_frame(&mut reader).await.unwrap());
        }
        writer.await.unwrap();
        assert_eq!(
            got,
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]
        );
    }
}
