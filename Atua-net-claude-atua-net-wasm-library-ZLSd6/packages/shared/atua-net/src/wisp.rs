//! Wisp v1 protocol client — frame codec + WebSocket-backed client.
//!
//! Clean-room implementation from the published Wisp v1 protocol spec.
//! Manages multiplexed TCP streams over a single WebSocket connection.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

// ─── Frame Codec ─────────────────────────────────────────────────

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    Connect = 0x01,
    Data = 0x02,
    Continue = 0x03,
    Close = 0x04,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StreamType {
    Tcp = 0x01,
    Udp = 0x02,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WispFrame {
    Connect {
        stream_id: u32,
        stream_type: StreamType,
        port: u16,
        hostname: String,
    },
    Data {
        stream_id: u32,
        payload: Vec<u8>,
    },
    Continue {
        stream_id: u32,
        buffer_remaining: u32,
    },
    Close {
        stream_id: u32,
        reason: u8,
    },
}

impl WispFrame {
    /// Serialize to bytes for sending over WebSocket.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            WispFrame::Connect { stream_id, stream_type, port, hostname } => {
                let hostname_bytes = hostname.as_bytes();
                let mut buf = Vec::with_capacity(5 + 3 + hostname_bytes.len());
                buf.push(FrameType::Connect as u8);
                buf.extend_from_slice(&stream_id.to_le_bytes());
                buf.push(*stream_type as u8);
                buf.extend_from_slice(&port.to_le_bytes());
                buf.extend_from_slice(hostname_bytes);
                buf
            }
            WispFrame::Data { stream_id, payload } => {
                let mut buf = Vec::with_capacity(5 + payload.len());
                buf.push(FrameType::Data as u8);
                buf.extend_from_slice(&stream_id.to_le_bytes());
                buf.extend_from_slice(payload);
                buf
            }
            WispFrame::Continue { stream_id, buffer_remaining } => {
                let mut buf = Vec::with_capacity(9);
                buf.push(FrameType::Continue as u8);
                buf.extend_from_slice(&stream_id.to_le_bytes());
                buf.extend_from_slice(&buffer_remaining.to_le_bytes());
                buf
            }
            WispFrame::Close { stream_id, reason } => {
                let mut buf = Vec::with_capacity(6);
                buf.push(FrameType::Close as u8);
                buf.extend_from_slice(&stream_id.to_le_bytes());
                buf.push(*reason);
                buf
            }
        }
    }

    /// Parse from bytes received from WebSocket.
    pub fn decode(data: &[u8]) -> Result<Self, String> {
        if data.len() < 5 {
            return Err(format!("wisp frame too short: {} bytes (min 5)", data.len()));
        }

        let frame_type = data[0];
        let stream_id = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let payload = &data[5..];

        match frame_type {
            0x01 => {
                // CONNECT: stream_type(1) + port(2) + hostname(rest)
                if payload.len() < 3 {
                    return Err(format!("CONNECT payload too short: {} bytes (min 3)", payload.len()));
                }
                let stream_type = match payload[0] {
                    0x01 => StreamType::Tcp,
                    0x02 => StreamType::Udp,
                    other => return Err(format!("unknown stream type: 0x{:02x}", other)),
                };
                let port = u16::from_le_bytes([payload[1], payload[2]]);
                let hostname = String::from_utf8(payload[3..].to_vec())
                    .map_err(|e| format!("invalid hostname UTF-8: {}", e))?;
                Ok(WispFrame::Connect { stream_id, stream_type, port, hostname })
            }
            0x02 => {
                // DATA: payload is raw bytes (can be empty)
                Ok(WispFrame::Data { stream_id, payload: payload.to_vec() })
            }
            0x03 => {
                // CONTINUE: buffer_remaining(4)
                if payload.len() != 4 {
                    return Err(format!("CONTINUE payload must be 4 bytes, got {}", payload.len()));
                }
                let buffer_remaining = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(WispFrame::Continue { stream_id, buffer_remaining })
            }
            0x04 => {
                // CLOSE: reason(1)
                if payload.len() != 1 {
                    return Err(format!("CLOSE payload must be 1 byte, got {}", payload.len()));
                }
                Ok(WispFrame::Close { stream_id, reason: payload[0] })
            }
            other => {
                Err(format!("unknown wisp frame type: 0x{:02x}", other))
            }
        }
    }
}

// ─── WispClient ──────────────────────────────────────────────────

struct StreamState {
    data_tx: mpsc::UnboundedSender<Result<Vec<u8>, String>>,
    buffer_remaining: u32,
    buffer_notify: Rc<tokio::sync::Notify>,
}

pub struct WispClient {
    ws: web_sys::WebSocket,
    streams: Rc<RefCell<HashMap<u32, StreamState>>>,
    next_stream_id: RefCell<u32>,
    initial_buffer_size: Rc<RefCell<u32>>,
    // Prevent closures from being dropped
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _onerror: Closure<dyn FnMut(web_sys::ErrorEvent)>,
    _onclose: Closure<dyn FnMut(web_sys::CloseEvent)>,
}

impl WispClient {
    pub async fn connect(url: &str) -> Result<Rc<Self>, String> {
        let ws = web_sys::WebSocket::new(url)
            .map_err(|e| format!("WebSocket open failed: {:?}", e))?;
        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let streams: Rc<RefCell<HashMap<u32, StreamState>>> = Rc::new(RefCell::new(HashMap::new()));
        let initial_buffer_size: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));

        // Channel for initial CONTINUE handshake
        let (ready_tx, ready_rx) = oneshot::channel::<u32>();
        let ready_tx = Rc::new(RefCell::new(Some(ready_tx)));

        // onmessage — decode frames and dispatch
        let streams_clone = streams.clone();
        let buf_size_clone = initial_buffer_size.clone();
        let ready_clone = ready_tx.clone();
        let onmessage = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            let data = event.data();
            let buffer = match data.dyn_into::<js_sys::ArrayBuffer>() {
                Ok(b) => b,
                Err(_) => return, // ignore non-binary messages
            };
            let array = js_sys::Uint8Array::new(&buffer);
            let mut bytes = vec![0u8; array.length() as usize];
            array.copy_to(&mut bytes);

            let frame = match WispFrame::decode(&bytes) {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("[wisp] malformed frame: {}", e);
                    return;
                }
            };

            match frame {
                WispFrame::Data { stream_id, payload } => {
                    let streams = streams_clone.borrow();
                    if let Some(state) = streams.get(&stream_id) {
                        let _ = state.data_tx.send(Ok(payload));
                    }
                }
                WispFrame::Continue { stream_id: 0, buffer_remaining } => {
                    *buf_size_clone.borrow_mut() = buffer_remaining;
                    if let Some(tx) = ready_clone.borrow_mut().take() {
                        let _ = tx.send(buffer_remaining);
                    }
                }
                WispFrame::Continue { stream_id, buffer_remaining } => {
                    let mut streams = streams_clone.borrow_mut();
                    if let Some(state) = streams.get_mut(&stream_id) {
                        state.buffer_remaining = buffer_remaining;
                        state.buffer_notify.notify_one(); // wake blocked poll_write
                    }
                }
                WispFrame::Close { stream_id, reason } => {
                    let mut streams = streams_clone.borrow_mut();
                    if let Some(state) = streams.remove(&stream_id) {
                        if reason == 0x02 {
                            let _ = state.data_tx.send(Ok(vec![])); // EOF
                        } else {
                            let _ = state.data_tx.send(Err(format!("stream closed: reason 0x{:02x}", reason)));
                        }
                    }
                }
                WispFrame::Connect { .. } => {
                    // Server shouldn't send CONNECT to client — ignore
                }
            }
        });
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        // onerror — close all streams with error
        let streams_err = streams.clone();
        let onerror = Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(move |_event: web_sys::ErrorEvent| {
            let mut streams = streams_err.borrow_mut();
            for (_, state) in streams.drain() {
                let _ = state.data_tx.send(Err("wisp connection error".into()));
            }
        });
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        // onclose — close all streams with EOF
        let streams_close = streams.clone();
        let onclose = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |_event: web_sys::CloseEvent| {
            let mut streams = streams_close.borrow_mut();
            for (_, state) in streams.drain() {
                let _ = state.data_tx.send(Ok(vec![])); // EOF
            }
        });
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));

        // Wait for WebSocket to open
        let ws_clone = ws.clone();
        let open_promise = js_sys::Promise::new(&mut |resolve, _reject| {
            let onopen = Closure::<dyn FnMut()>::once(move || {
                resolve.call0(&JsValue::NULL).ok();
            });
            ws_clone.set_onopen(Some(onopen.as_ref().unchecked_ref()));
            onopen.forget();
        });
        wasm_bindgen_futures::JsFuture::from(open_promise).await
            .map_err(|_| "WebSocket open failed".to_string())?;

        // Wait for initial CONTINUE (stream_id=0) from server
        let buf_size = ready_rx.await
            .map_err(|_| "did not receive initial CONTINUE from wisp server".to_string())?;

        log::info!("[wisp] connected to {} (buffer_size={})", url, buf_size);

        Ok(Rc::new(Self {
            ws,
            streams,
            next_stream_id: RefCell::new(1),
            initial_buffer_size,
            _onmessage: onmessage,
            _onerror: onerror,
            _onclose: onclose,
        }))
    }

    pub fn open_stream(&self, host: &str, port: u16) -> Result<(u32, mpsc::UnboundedReceiver<Result<Vec<u8>, String>>, Rc<tokio::sync::Notify>), String> {
        let stream_id = {
            let mut id = self.next_stream_id.borrow_mut();
            let sid = *id;
            *id += 1;
            sid
        };

        let (data_tx, data_rx) = mpsc::unbounded_channel();
        let buffer_remaining = *self.initial_buffer_size.borrow();
        let buffer_notify = Rc::new(tokio::sync::Notify::new());

        self.streams.borrow_mut().insert(stream_id, StreamState {
            data_tx,
            buffer_remaining,
            buffer_notify: buffer_notify.clone(),
        });

        // Send CONNECT frame
        let frame = WispFrame::Connect {
            stream_id,
            stream_type: StreamType::Tcp,
            port,
            hostname: host.to_string(),
        };
        self.send_frame(&frame)?;

        log::debug!("[wisp] opened stream {} → {}:{} (buffer={})", stream_id, host, port, buffer_remaining);

        Ok((stream_id, data_rx, buffer_notify))
    }

    /// Send DATA frame. Returns Ok(true) if sent, Ok(false) if buffer full (caller must wait).
    pub fn send_data(&self, stream_id: u32, payload: &[u8]) -> Result<bool, String> {
        // Check flow control BEFORE sending
        {
            let streams = self.streams.borrow();
            let state = streams.get(&stream_id).ok_or("stream not found")?;
            if state.buffer_remaining == 0 {
                return Ok(false); // buffer exhausted, wait for CONTINUE
            }
        }

        let frame = WispFrame::Data {
            stream_id,
            payload: payload.to_vec(),
        };
        self.send_frame(&frame)?;

        // Decrement buffer_remaining after successful send
        let mut streams = self.streams.borrow_mut();
        if let Some(state) = streams.get_mut(&stream_id) {
            state.buffer_remaining = state.buffer_remaining.saturating_sub(1);
        }

        Ok(true)
    }

    pub fn close_stream(&self, stream_id: u32) {
        let frame = WispFrame::Close {
            stream_id,
            reason: 0x02, // voluntary
        };
        let _ = self.send_frame(&frame);
        self.streams.borrow_mut().remove(&stream_id);
    }

    fn send_frame(&self, frame: &WispFrame) -> Result<(), String> {
        let bytes = frame.encode();
        self.ws.send_with_u8_array(&bytes)
            .map_err(|e| format!("WebSocket send failed: {:?}", e))
    }

    /// Number of active streams in this client.
    pub fn stream_count(&self) -> usize {
        self.streams.borrow().len()
    }

    /// WebSocket ready state (0=CONNECTING, 1=OPEN, 2=CLOSING, 3=CLOSED).
    pub fn ws_ready_state(&self) -> u16 {
        self.ws.ready_state()
    }
}

// ─── Thread-Local Client Pool ────────────────────────────────────

thread_local! {
    pub static WISP_CLIENTS: RefCell<HashMap<String, Rc<WispClient>>> = RefCell::new(HashMap::new());
}

pub async fn get_or_create(url: &str) -> Result<Rc<WispClient>, String> {
    // Check for existing client
    let existing = WISP_CLIENTS.with(|c| {
        c.borrow().get(url).cloned()
    });

    if let Some(client) = existing {
        // Check if WebSocket is still open
        if client.ws.ready_state() == web_sys::WebSocket::OPEN {
            return Ok(client);
        }
        // Dead connection — remove and reconnect
        WISP_CLIENTS.with(|c| c.borrow_mut().remove(url));
    }

    let client = WispClient::connect(url).await?;
    WISP_CLIENTS.with(|c| c.borrow_mut().insert(url.to_string(), client.clone()));
    Ok(client)
}

// ─── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_encode_decode() {
        let frame = WispFrame::Connect {
            stream_id: 1,
            stream_type: StreamType::Tcp,
            port: 443,
            hostname: "example.com".into(),
        };
        let bytes = frame.encode();
        let decoded = WispFrame::decode(&bytes).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn connect_long_hostname() {
        let frame = WispFrame::Connect {
            stream_id: 42,
            stream_type: StreamType::Tcp,
            port: 8080,
            hostname: "a".repeat(1000),
        };
        let bytes = frame.encode();
        let decoded = WispFrame::decode(&bytes).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn connect_port_zero_and_max() {
        for port in [0u16, 65535] {
            let frame = WispFrame::Connect {
                stream_id: 1, stream_type: StreamType::Tcp, port, hostname: "h".into(),
            };
            let decoded = WispFrame::decode(&frame.encode()).unwrap();
            if let WispFrame::Connect { port: p, .. } = decoded { assert_eq!(p, port); }
        }
    }

    #[test]
    fn data_encode_decode_empty() {
        let frame = WispFrame::Data { stream_id: 5, payload: vec![] };
        let decoded = WispFrame::decode(&frame.encode()).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn data_encode_decode_1byte() {
        let frame = WispFrame::Data { stream_id: 5, payload: vec![0xAB] };
        let decoded = WispFrame::decode(&frame.encode()).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn data_encode_decode_64kb() {
        let frame = WispFrame::Data { stream_id: 5, payload: vec![0x42; 65536] };
        let decoded = WispFrame::decode(&frame.encode()).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn continue_encode_decode() {
        for buf in [0u32, 1, u32::MAX] {
            let frame = WispFrame::Continue { stream_id: 0, buffer_remaining: buf };
            let decoded = WispFrame::decode(&frame.encode()).unwrap();
            assert_eq!(frame, decoded);
        }
    }

    #[test]
    fn close_encode_decode_all_reasons() {
        for reason in [0x01, 0x02, 0x03, 0x41, 0x42, 0x43, 0x44, 0x47] {
            let frame = WispFrame::Close { stream_id: 10, reason };
            let decoded = WispFrame::decode(&frame.encode()).unwrap();
            assert_eq!(frame, decoded);
        }
    }

    #[test]
    fn stream_id_little_endian() {
        let frame = WispFrame::Data { stream_id: 0x04030201, payload: vec![] };
        let bytes = frame.encode();
        // bytes[1..5] should be LE: 01 02 03 04
        assert_eq!(bytes[1], 0x01);
        assert_eq!(bytes[2], 0x02);
        assert_eq!(bytes[3], 0x03);
        assert_eq!(bytes[4], 0x04);
    }

    #[test]
    fn reject_truncated_frame() {
        assert!(WispFrame::decode(&[]).is_err());
        assert!(WispFrame::decode(&[0x01]).is_err());
        assert!(WispFrame::decode(&[0x01, 0, 0, 0]).is_err()); // 4 bytes, need 5
    }

    #[test]
    fn reject_short_connect_payload() {
        // 5 header bytes + 2 payload bytes (need at least 3)
        let bytes = [0x01, 1, 0, 0, 0, 0x01, 0x00];
        assert!(WispFrame::decode(&bytes).is_err());
    }

    #[test]
    fn reject_wrong_continue_payload_size() {
        // CONTINUE with 3 bytes payload (need exactly 4)
        let bytes = [0x03, 0, 0, 0, 0, 1, 2, 3];
        assert!(WispFrame::decode(&bytes).is_err());
        // CONTINUE with 5 bytes payload
        let bytes = [0x03, 0, 0, 0, 0, 1, 2, 3, 4, 5];
        assert!(WispFrame::decode(&bytes).is_err());
    }

    #[test]
    fn reject_wrong_close_payload_size() {
        // CLOSE with 0 bytes payload (need exactly 1)
        let bytes = [0x04, 0, 0, 0, 0];
        assert!(WispFrame::decode(&bytes).is_err());
        // CLOSE with 2 bytes payload
        let bytes = [0x04, 0, 0, 0, 0, 1, 2];
        assert!(WispFrame::decode(&bytes).is_err());
    }

    #[test]
    fn unknown_frame_type_errors() {
        let bytes = [0xFF, 0, 0, 0, 0];
        assert!(WispFrame::decode(&bytes).is_err());
    }
}
