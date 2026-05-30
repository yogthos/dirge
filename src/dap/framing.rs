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
