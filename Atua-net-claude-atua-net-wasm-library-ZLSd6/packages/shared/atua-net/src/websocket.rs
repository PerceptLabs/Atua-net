//! Minimal WebSocket client (RFC 6455) over AsyncRead + AsyncWrite.
//! Hand-rolled because tokio-tungstenite doesn't compile to wasm32-unknown-unknown.

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const OPCODE_TEXT: u8 = 0x01;
const OPCODE_BINARY: u8 = 0x02;
const OPCODE_CLOSE: u8 = 0x08;
const OPCODE_PING: u8 = 0x09;
const OPCODE_PONG: u8 = 0x0A;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-5AB4AE863DFF";

/// A received WebSocket frame.
pub struct WsFrame {
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Write a WebSocket frame. Client frames MUST be masked (RFC 6455 §5.3).
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    opcode: u8,
    payload: &[u8],
) -> Result<(), String> {
    let mut header = Vec::with_capacity(14 + payload.len());

    // FIN bit + opcode
    header.push(0x80 | opcode);

    // Mask bit (1) + payload length
    let len = payload.len();
    if len < 126 {
        header.push(0x80 | len as u8);
    } else if len <= 65535 {
        header.push(0x80 | 126);
        header.push((len >> 8) as u8);
        header.push(len as u8);
    } else {
        header.push(0x80 | 127);
        for i in (0..8).rev() {
            header.push((len >> (i * 8)) as u8);
        }
    }

    // Mask key — MUST be unpredictable (RFC 6455 §5.3)
    let mut mask = [0u8; 4];
    getrandom::getrandom(&mut mask).map_err(|e| format!("mask rng failed: {}", e))?;
    header.extend_from_slice(&mask);

    // Masked payload
    let mut masked = payload.to_vec();
    for (i, byte) in masked.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    header.extend_from_slice(&masked);

    writer
        .write_all(&header)
        .await
        .map_err(|e| format!("ws write failed: {}", e))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("ws flush failed: {}", e))?;
    Ok(())
}

/// Read a WebSocket frame.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<WsFrame, String> {
    let mut hdr = [0u8; 2];
    reader
        .read_exact(&mut hdr)
        .await
        .map_err(|e| format!("ws read header failed: {}", e))?;

    let opcode = hdr[0] & 0x0F;
    let masked = (hdr[1] & 0x80) != 0;
    let len_byte = hdr[1] & 0x7F;

    let payload_len: usize = if len_byte < 126 {
        len_byte as usize
    } else if len_byte == 126 {
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).await.map_err(|e| format!("ws read len16: {}", e))?;
        u16::from_be_bytes(buf) as usize
    } else {
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).await.map_err(|e| format!("ws read len64: {}", e))?;
        u64::from_be_bytes(buf) as usize
    };

    let mask_key = if masked {
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).await.map_err(|e| format!("ws read mask: {}", e))?;
        Some(buf)
    } else {
        None
    };

    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader
            .read_exact(&mut payload)
            .await
            .map_err(|e| format!("ws read payload: {}", e))?;
    }

    if let Some(mask) = mask_key {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }

    Ok(WsFrame { opcode, payload })
}

/// Perform the WebSocket upgrade handshake over an existing stream.
/// Generates a random Sec-WebSocket-Key and verifies Sec-WebSocket-Accept.
pub async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    path: &str,
) -> Result<(), String> {
    // Generate random 16-byte key, base64-encode
    let mut key_bytes = [0u8; 16];
    getrandom::getrandom(&mut key_bytes).map_err(|e| format!("key rng failed: {}", e))?;
    let key = base64::engine::general_purpose::STANDARD.encode(key_bytes);

    let request = format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n",
        path, host, key
    );

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("ws handshake write failed: {}", e))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("ws handshake flush failed: {}", e))?;

    // Read response headers
    let mut response = Vec::with_capacity(1024);
    let mut buf = [0u8; 1];
    loop {
        stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("ws handshake read failed: {}", e))?;
        response.push(buf[0]);
        if response.len() >= 4 && &response[response.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if response.len() > 8192 {
            return Err("ws handshake response too large".into());
        }
    }

    let response_str = String::from_utf8_lossy(&response);
    if !response_str.contains("101") {
        return Err(format!(
            "ws handshake failed: expected 101, got: {}",
            response_str.lines().next().unwrap_or("empty")
        ));
    }

    // Verify Sec-WebSocket-Accept
    let expected_accept = {
        let mut input = key.clone();
        input.push_str(WS_GUID);
        let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, input.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(hash.as_ref())
    };

    // Parse Sec-WebSocket-Accept from response headers
    let accept_value = response_str
        .lines()
        .find(|line| line.to_lowercase().starts_with("sec-websocket-accept:"))
        .and_then(|line| line.splitn(2, ':').nth(1))
        .map(|v| v.trim().to_string());

    match accept_value {
        Some(actual) if actual == expected_accept => Ok(()),
        Some(actual) => Err(format!(
            "ws handshake: Sec-WebSocket-Accept mismatch: expected {}, got {}",
            expected_accept, actual
        )),
        None => Err("ws handshake: missing Sec-WebSocket-Accept header".into()),
    }
}

/// Write a text message.
pub async fn send_text<W: AsyncWrite + Unpin>(writer: &mut W, text: &str) -> Result<(), String> {
    write_frame(writer, OPCODE_TEXT, text.as_bytes()).await
}

/// Write a close frame.
pub async fn send_close<W: AsyncWrite + Unpin>(writer: &mut W) -> Result<(), String> {
    write_frame(writer, OPCODE_CLOSE, &[]).await
}

/// Read the next text or binary message, handling pings automatically.
pub async fn recv_message<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<Option<String>, String> {
    loop {
        let frame = read_frame(stream).await?;
        match frame.opcode {
            OPCODE_TEXT => {
                return Ok(Some(
                    String::from_utf8(frame.payload)
                        .map_err(|e| format!("invalid utf8 in ws text: {}", e))?,
                ));
            }
            OPCODE_BINARY => {
                return Ok(Some(
                    String::from_utf8_lossy(&frame.payload).into_owned(),
                ));
            }
            OPCODE_CLOSE => return Ok(None),
            OPCODE_PING => {
                write_frame(stream, OPCODE_PONG, &frame.payload).await?;
            }
            _ => {}
        }
    }
}
