//! WispStream: tokio AsyncRead + AsyncWrite over either JS callbacks or native Rust Wisp client.
//!
//! Two backends:
//!   - JS path: spawn_local + mpsc channels + JS Wisp callbacks (existing)
//!   - Native path: Rust WispClient with web_sys::WebSocket (new)
//!
//! Both produce the same WispStream with identical AsyncRead/AsyncWrite behavior.

use bytes::BytesMut;
use js_sys::{Function, Promise, Uint8Array};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

/// Call a JS function and await the result if it's a Promise.
pub async fn call_js(func: &Function, args: &[JsValue]) -> Result<JsValue, String> {
    let this = JsValue::NULL;
    let result = match args.len() {
        0 => func.call0(&this),
        1 => func.call1(&this, &args[0]),
        2 => func.call2(&this, &args[0], &args[1]),
        _ => {
            let js_args = js_sys::Array::new();
            for arg in args {
                js_args.push(arg);
            }
            func.apply(&this, &js_args)
        }
    }
    .map_err(|e| format!("JS call failed: {:?}", e))?;

    if result.is_instance_of::<Promise>() {
        JsFuture::from(Promise::from(result))
            .await
            .map_err(|e| format!("JS promise rejected: {:?}", e))
    } else {
        Ok(result)
    }
}

/// A write request sent through the channel (JS path only).
struct WriteRequest {
    data: Vec<u8>,
    done: oneshot::Sender<Result<(), String>>,
}

/// Write backend — determines how poll_write and poll_shutdown work.
enum WriteBackend {
    /// JS callback path: writes go through mpsc → background task → JS wisp_send
    JsCallbacks {
        write_tx: mpsc::Sender<WriteRequest>,
        close_fn: Function,
        stream_id: JsValue,
    },
    /// Native Rust Wisp client: writes call WispClient::send_data directly
    Native {
        stream_id: u32,
        client: std::rc::Rc<crate::wisp::WispClient>,
        buffer_notify: std::rc::Rc<tokio::sync::Notify>,
        waiting_for_continue: bool,
    },
}

/// A tokio-compatible async stream — works with either JS or native Wisp backend.
pub struct WispStream {
    /// Receiver for data (both paths feed this via mpsc channel).
    read_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
    /// Buffered data not yet consumed by AsyncRead.
    read_buf: BytesMut,
    /// True once read channel has been closed.
    eof: bool,
    /// Pending write completion (JS path only).
    pending_write: Option<oneshot::Receiver<Result<(), String>>>,
    /// Write backend.
    backend: WriteBackend,
}

impl WispStream {
    /// JS callback path — existing constructor. DO NOT MODIFY BEHAVIOR.
    pub fn new(
        stream_id: JsValue,
        wisp_send: Function,
        wisp_recv: Function,
        wisp_close: Function,
    ) -> Self {
        // Background recv loop
        let (read_tx, read_rx) = mpsc::channel::<Result<Vec<u8>, String>>(64);
        let recv_sid = stream_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let result = call_js(&wisp_recv, &[recv_sid.clone()]).await;
                match result {
                    Ok(val) => {
                        let arr = Uint8Array::new(&val);
                        let mut data = vec![0u8; arr.length() as usize];
                        arr.copy_to(&mut data);
                        let is_eof = data.is_empty();
                        if read_tx.send(Ok(data)).await.is_err() {
                            break;
                        }
                        if is_eof {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = read_tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        // Background write loop
        let (write_tx, mut write_rx) = mpsc::channel::<WriteRequest>(64);
        let send_sid = stream_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(req) = write_rx.recv().await {
                let arr = Uint8Array::from(req.data.as_slice());
                let result = call_js(&wisp_send, &[send_sid.clone(), arr.into()]).await;
                let _ = req.done.send(result.map(|_| ()));
            }
        });

        Self {
            read_rx,
            read_buf: BytesMut::with_capacity(16384),
            eof: false,
            pending_write: None,
            backend: WriteBackend::JsCallbacks {
                write_tx,
                close_fn: wisp_close,
                stream_id,
            },
        }
    }

    /// Native Rust Wisp client path.
    pub fn from_native(
        stream_id: u32,
        data_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
        client: std::rc::Rc<crate::wisp::WispClient>,
        buffer_notify: std::rc::Rc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            read_rx: data_rx,
            read_buf: BytesMut::with_capacity(16384),
            eof: false,
            pending_write: None,
            backend: WriteBackend::Native {
                stream_id,
                client,
                buffer_notify,
                waiting_for_continue: false,
            },
        }
    }
}

impl AsyncRead for WispStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();

        if !me.read_buf.is_empty() {
            let to_copy = me.read_buf.len().min(buf.remaining());
            buf.put_slice(&me.read_buf[..to_copy]);
            let _ = me.read_buf.split_to(to_copy);
            return Poll::Ready(Ok(()));
        }

        if me.eof {
            return Poll::Ready(Ok(()));
        }

        match me.read_rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(data))) => {
                if data.is_empty() {
                    me.eof = true;
                    return Poll::Ready(Ok(()));
                }
                let to_copy = data.len().min(buf.remaining());
                buf.put_slice(&data[..to_copy]);
                if to_copy < data.len() {
                    me.read_buf.extend_from_slice(&data[to_copy..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Some(Err(e))) => {
                me.eof = true;
                Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)))
            }
            Poll::Ready(None) => {
                me.eof = true;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for WispStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();

        match &mut me.backend {
            WriteBackend::Native { stream_id, client, buffer_notify, waiting_for_continue } => {
                // Native path: synchronous WebSocket.send with flow control
                match client.send_data(*stream_id, buf) {
                    Ok(true) => {
                        *waiting_for_continue = false;
                        Poll::Ready(Ok(buf.len()))
                    }
                    Ok(false) => {
                        // Buffer full — spawn waiter only if not already waiting
                        if !*waiting_for_continue {
                            *waiting_for_continue = true;
                            let notify = buffer_notify.clone();
                            let waker = cx.waker().clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                notify.notified().await;
                                waker.wake();
                            });
                        }
                        Poll::Pending
                    }
                    Err(e) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
                }
            }
            WriteBackend::JsCallbacks { write_tx, .. } => {
                // JS path: existing channel-based write

                // Check if a previous write is still pending.
                if let Some(pending) = &mut me.pending_write {
                    match Pin::new(pending).poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Ok(Ok(()))) => {
                            me.pending_write = None;
                        }
                        Poll::Ready(Ok(Err(e))) => {
                            me.pending_write = None;
                            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)));
                        }
                        Poll::Ready(Err(_)) => {
                            me.pending_write = None;
                            return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "write channel closed")));
                        }
                    }
                }

                let (done_tx, done_rx) = oneshot::channel();
                let req = WriteRequest {
                    data: buf.to_vec(),
                    done: done_tx,
                };

                match write_tx.try_send(req) {
                    Ok(()) => {
                        me.pending_write = Some(done_rx);
                        if let Some(pending) = &mut me.pending_write {
                            match Pin::new(pending).poll(cx) {
                                Poll::Ready(Ok(Ok(()))) => {
                                    me.pending_write = None;
                                    Poll::Ready(Ok(buf.len()))
                                }
                                Poll::Ready(Ok(Err(e))) => {
                                    me.pending_write = None;
                                    Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)))
                                }
                                Poll::Ready(Err(_)) => {
                                    me.pending_write = None;
                                    Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "write channel closed")))
                                }
                                Poll::Pending => {
                                    Poll::Ready(Ok(buf.len()))
                                }
                            }
                        } else {
                            Poll::Ready(Ok(buf.len()))
                        }
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "write channel closed")))
                    }
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        match &me.backend {
            WriteBackend::JsCallbacks { close_fn, stream_id, .. } => {
                let _ = close_fn.call1(&JsValue::NULL, stream_id);
            }
            WriteBackend::Native { stream_id, client, .. } => {
                client.close_stream(*stream_id);
            }
        }
        Poll::Ready(Ok(()))
    }
}

// ─── TokioIo Adapter ────────────────────────────────────────────

pin_project_lite::pin_project! {
    pub struct TokioIo<T> {
        #[pin]
        inner: T,
    }
}

impl<T> TokioIo<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: AsyncRead> hyper::rt::Read for TokioIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let n = unsafe {
            let mut tbuf = ReadBuf::uninit(buf.as_mut());
            match AsyncRead::poll_read(self.project().inner, cx, &mut tbuf) {
                Poll::Ready(Ok(())) => tbuf.filled().len(),
                other => return other,
            }
        };
        unsafe { buf.advance(n) };
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncWrite> hyper::rt::Write for TokioIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(self.project().inner, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(self.project().inner, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(self.project().inner, cx)
    }
}
