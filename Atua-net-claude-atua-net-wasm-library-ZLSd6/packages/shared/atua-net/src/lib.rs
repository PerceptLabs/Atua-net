mod websocket;
mod wisp;
mod wisp_stream;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use log::{info, debug, warn, error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wisp_stream::TokioIo;
use wasm_bindgen::prelude::*;

use wisp_stream::{call_js, WispStream};

// ─── Initialization ──────────────────────────────────────────────

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Debug);
}

// ─── TLS Configuration ──────────────────────────────────────────

fn tls_config() -> Arc<rustls::ClientConfig> {
    let root_store =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Arc::new(config)
}

fn tls_config_raw() -> Arc<rustls::ClientConfig> {
    let root_store =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    // No ALPN for raw TLS streams — the caller manages the application protocol
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    )
}

thread_local! {
    static TLS_CONFIG: Arc<rustls::ClientConfig> = tls_config();
    static TLS_CONFIG_RAW: Arc<rustls::ClientConfig> = tls_config_raw();
    static COOKIE_JAR: RefCell<cookie_store::CookieStore> = RefCell::new(cookie_store::CookieStore::default());
}

// ─── WASM Executor for hyper HTTP/2 ──────────────────────────────

#[derive(Clone)]
struct WasmExecutor;

impl<F> hyper::rt::Executor<F> for WasmExecutor
where
    F: std::future::Future + 'static,
{
    fn execute(&self, fut: F) {
        wasm_bindgen_futures::spawn_local(async move {
            fut.await;
        });
    }
}

// ─── URL Parsing (via `url` crate — WHATWG compliant) ────────────

fn parse_url(raw: &str) -> Result<(String, u16, String), String> {
    let parsed = url::Url::parse(raw.trim())
        .map_err(|e| format!("invalid URL: {}", e))?;

    if parsed.scheme() != "https" {
        return Err(format!("only HTTPS URLs are supported, got: {}", parsed.scheme()));
    }

    let host = parsed.host_str()
        .ok_or("no host in URL")?
        .to_string();

    let port = parsed.port().unwrap_or(443);

    let path = {
        let p = &parsed[url::Position::BeforePath..];
        if p.is_empty() { "/".to_string() } else { p.to_string() }
    };

    Ok((host, port, path))
}

// ─── JSON Header Parsing (via `serde_json`) ──────────────────────

fn parse_headers_json(json: &str) -> Result<Vec<(String, String)>, String> {
    if json.is_empty() || json == "{}" {
        return Ok(Vec::new());
    }
    let map: std::collections::HashMap<String, String> = serde_json::from_str(json)
        .map_err(|e| format!("invalid headers JSON: {}", e))?;
    Ok(map.into_iter().collect())
}

// ─── Custom TLS Config Builder ───────────────────────────────

fn build_custom_tls_config(
    custom_ca_pem: &Option<String>,
    tls_config_json: &Option<String>,
) -> Result<Option<Arc<rustls::ClientConfig>>, JsValue> {
    if custom_ca_pem.is_none() && tls_config_json.is_none() {
        return Ok(None);
    }

    let mut root_store =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Add custom CA certificates from PEM
    if let Some(pem) = custom_ca_pem {
        let mut reader = std::io::BufReader::new(pem.as_bytes());
        for cert in rustls_pemfile::certs(&mut reader) {
            match cert {
                Ok(c) => {
                    root_store.add(c).map_err(|e| JsValue::from_str(&format!("failed to add CA cert: {}", e)))?;
                }
                Err(e) => {
                    return Err(JsValue::from_str(&format!("failed to parse PEM cert: {}", e)));
                }
            }
        }
    }

    // Apply TLS configuration overrides
    let mut config = if let Some(json) = tls_config_json {
        #[derive(serde::Deserialize)]
        struct TlsOpts {
            #[serde(rename = "minVersion")]
            min_version: Option<String>,
            alpn: Option<Vec<String>>,
        }
        let opts: TlsOpts = serde_json::from_str(json)
            .map_err(|e| JsValue::from_str(&format!("invalid tls config: {}", e)))?;

        let c = if opts.min_version.as_deref() == Some("1.3") {
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_root_certificates(root_store)
                .with_no_client_auth()
        } else {
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        let mut c = c;
        if let Some(alpn) = opts.alpn {
            c.alpn_protocols = alpn.into_iter().map(|s| s.into_bytes()).collect();
        } else {
            c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        }
        c
    } else {
        let mut c = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        c
    };

    Ok(Some(Arc::new(config)))
}

// ─── Certificate Pinning ─────────────────────────────────────

fn verify_pins(
    tls_stream: &tokio_rustls::client::TlsStream<WispStream>,
    host: &str,
    pins_json: &str,
) -> Result<(), String> {
    let pins: HashMap<String, Vec<String>> = serde_json::from_str(pins_json)
        .map_err(|e| format!("invalid pins JSON: {}", e))?;

    let host_pins = match pins.get(host) {
        Some(p) => p,
        None => return Ok(()), // no pins for this host
    };

    let certs = tls_stream.get_ref().1.peer_certificates()
        .ok_or("no peer certificates")?;

    if certs.is_empty() {
        return Err("no peer certificates for pin verification".into());
    }

    // Extract SPKI from leaf certificate
    let leaf = &certs[0];
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|e| format!("failed to parse certificate: {}", e))?;

    let spki_bytes = parsed.tbs_certificate.subject_pki.raw;
    let hash = ring::digest::digest(&ring::digest::SHA256, spki_bytes);
    let pin = format!("sha256/{}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, hash.as_ref()));

    if host_pins.contains(&pin) {
        debug!("[pin] certificate pin matched for {}", host);
        Ok(())
    } else {
        error!("[pin] certificate pin mismatch for {}: got {}", host, pin);
        Err(format!("certificate pin mismatch for {}: expected one of {:?}, got {}", host, host_pins, pin))
    }
}

// ─── Dual-Path Stream Creation ───────────────────────────────

/// Create a WispStream from either the native Rust path or the JS callback path.
async fn create_wisp_stream(
    host: &str,
    port: u16,
    use_native: bool,
    wisp_url: &Option<String>,
    wisp_open: &Option<js_sys::Function>,
    wisp_send: &Option<js_sys::Function>,
    wisp_recv: &Option<js_sys::Function>,
    wisp_close: &Option<js_sys::Function>,
) -> Result<WispStream, String> {
    if use_native {
        let wurl = wisp_url.as_deref().ok_or("wisp_url required for native wisp")?;
        let client = wisp::get_or_create(wurl).await?;
        let (sid, rx, notify) = client.open_stream(host, port)?;
        Ok(WispStream::from_native(sid, rx, client, notify))
    } else {
        let wo = wisp_open.as_ref().ok_or("wisp_open callback required")?;
        let stream_id = call_js(wo, &[JsValue::from_str(host), JsValue::from_f64(port as f64)]).await?;
        Ok(WispStream::new(
            stream_id,
            wisp_send.clone().ok_or("wisp_send required")?,
            wisp_recv.clone().ok_or("wisp_recv required")?,
            wisp_close.clone().ok_or("wisp_close required")?,
        ))
    }
}

// ─── Connection Pool ─────────────────────────────────────────

enum PooledSender {
    H1(hyper::client::conn::http1::SendRequest<Full<Bytes>>),
    H2(hyper::client::conn::http2::SendRequest<Full<Bytes>>),
}

struct PooledConnection {
    sender: PooledSender,
    idle_since: f64,
}

thread_local! {
    static CONN_POOL: RefCell<HashMap<String, PooledConnection>> = RefCell::new(HashMap::new());
}

// ─── Stream Storage for atua_connect ─────────────────────────────

enum StoredStream {
    Tls(tokio_rustls::client::TlsStream<WispStream>),
    Raw(WispStream),
}

thread_local! {
    static STREAMS: RefCell<HashMap<String, StoredStream>> = RefCell::new(HashMap::new());
    static NEXT_STREAM_ID: RefCell<u64> = RefCell::new(1);
}

fn next_stream_key() -> String {
    NEXT_STREAM_ID.with(|id| {
        let mut id = id.borrow_mut();
        let key = format!("s{}", *id);
        *id += 1;
        key
    })
}

// ─── HTTPS Fetch ─────────────────────────────────────────────────

#[wasm_bindgen]
pub async fn atua_fetch(
    url: String,
    method: String,
    headers_json: String,
    body: Option<Vec<u8>>,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    timeout_ms: Option<u32>,
    max_redirects: Option<u8>,
    use_cookies: Option<bool>,
    pins_json: Option<String>,
    custom_ca_pem: Option<String>,
    tls_config_json: Option<String>,
    use_native_wisp: Option<bool>,
    wisp_url: Option<String>,
) -> Result<JsValue, JsValue> {
    // Build custom TLS config if needed
    let custom_tls = build_custom_tls_config(&custom_ca_pem, &tls_config_json)?;

    let fetch_future = atua_fetch_with_redirects(
        url.clone(), method.clone(), headers_json, body,
        wisp_send, wisp_recv, wisp_open, wisp_close,
        max_redirects.unwrap_or(10),
        use_cookies.unwrap_or(false),
        pins_json, custom_tls,
        use_native_wisp.unwrap_or(false), wisp_url,
    );

    let result = if let Some(ms) = timeout_ms {
        match wasmtimer::tokio::timeout(
            std::time::Duration::from_millis(ms as u64),
            fetch_future,
        ).await {
            Ok(inner) => inner,
            Err(_) => {
                warn!("[fetch] {} {} timed out after {}ms", method, url, ms);
                Err(format!("timeout after {}ms", ms))
            }
        }
    } else {
        fetch_future.await
    };

    result.map_err(|e| JsValue::from_str(&e))
}

async fn atua_fetch_with_redirects(
    url: String,
    method: String,
    mut headers_json: String,
    body: Option<Vec<u8>>,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    max_redirects: u8,
    use_cookies: bool,
    pins_json: Option<String>,
    custom_tls: Option<Arc<rustls::ClientConfig>>,
    use_native_wisp: bool,
    wisp_url: Option<String>,
) -> Result<JsValue, String> {
    let mut current_url = url;
    let mut current_method = method;
    let mut current_body = body;
    let mut redirects_remaining = max_redirects;

    loop {
        let result = atua_fetch_inner(
            current_url.clone(), current_method.clone(), headers_json.clone(),
            current_body.clone(), wisp_send.clone(), wisp_recv.clone(),
            wisp_open.clone(), wisp_close.clone(), use_cookies, &pins_json,
            &custom_tls, use_native_wisp, &wisp_url,
        ).await?;

        // Check for redirect status
        let status = js_sys::Reflect::get(&result, &"status".into())
            .map_err(|_| "no status")?
            .as_f64()
            .unwrap_or(0.0) as u16;

        if ![301, 302, 303, 307, 308].contains(&status) {
            return Ok(result);
        }

        if redirects_remaining == 0 {
            return Err(format!("too many redirects (max {})", max_redirects));
        }
        redirects_remaining -= 1;

        // Get Location header
        let headers = js_sys::Reflect::get(&result, &"headers".into())
            .map_err(|_| "no headers")?;
        let location = js_sys::Reflect::get(&headers, &"location".into())
            .ok()
            .and_then(|v| v.as_string())
            .ok_or("redirect without Location header")?;

        // Resolve relative URL
        let base = url::Url::parse(&current_url)
            .map_err(|e| format!("invalid base URL: {}", e))?;
        let resolved = base.join(&location)
            .map_err(|e| format!("invalid redirect URL: {}", e))?;

        debug!("[redirect] {} → {}", current_url, resolved);

        // Strip sensitive headers on cross-origin redirects
        let current_origin = url::Url::parse(&current_url).ok().map(|u| u.origin());
        let redirect_origin = Some(resolved.origin());
        if current_origin != redirect_origin {
            let mut hdrs: HashMap<String, String> = serde_json::from_str(&headers_json)
                .unwrap_or_default();
            let sensitive = ["Authorization", "authorization", "Cookie", "cookie",
                           "Proxy-Authorization", "proxy-authorization"];
            let had_sensitive = sensitive.iter().any(|k| hdrs.contains_key(*k));
            for key in &sensitive {
                hdrs.remove(*key);
            }
            if had_sensitive {
                debug!("[redirect] stripped sensitive headers for cross-origin redirect");
            }
            headers_json = serde_json::to_string(&hdrs).unwrap_or_default();
        }

        current_url = resolved.to_string();

        // 301/302/303: switch to GET, drop body
        if [301, 302, 303].contains(&status) {
            current_method = "GET".to_string();
            current_body = None;
        }
        // 307/308: preserve method and body
    }
}

async fn atua_fetch_inner(
    url: String,
    method: String,
    headers_json: String,
    body: Option<Vec<u8>>,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    use_cookies: bool,
    pins_json: &Option<String>,
    custom_tls: &Option<Arc<rustls::ClientConfig>>,
    use_native_wisp: bool,
    wisp_url: &Option<String>,
) -> Result<JsValue, String> {
    let t_start = js_sys::Date::now();
    let (host, port, path) = parse_url(&url)?;

    info!("[fetch] {} {} ({}:{})", method, url, host, port);

    // ── Connection pooling: try to reuse existing connection ────
    let pool_key = format!("{}:{}", host, port);
    let mut t_stream = t_start;
    let mut t_tls = t_start;
    let mut from_pool = false;

    // Check pool for a live connection
    let mut pooled = CONN_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        if let Some(conn) = pool.remove(&pool_key) {
            let idle_ms = js_sys::Date::now() - conn.idle_since;
            if idle_ms < 60_000.0 {
                return Some(conn.sender);
            }
            debug!("[pool] discarding idle connection to {} (idle {:.0}ms)", pool_key, idle_ms);
        }
        None
    });

    // Check if pooled sender is still alive
    let mut sender: Option<PooledSender> = if let Some(mut ps) = pooled.take() {
        let alive = match &mut ps {
            PooledSender::H1(s) => s.ready().await.is_ok(),
            PooledSender::H2(s) => s.ready().await.is_ok(),
        };
        if alive {
            debug!("[pool] reusing connection to {}", pool_key);
            from_pool = true;
            Some(ps)
        } else {
            debug!("[pool] pooled connection to {} is dead, opening fresh", pool_key);
            None
        }
    } else {
        None
    };

    // Open fresh connection if not from pool
    if sender.is_none() {
        let wisp = create_wisp_stream(
            &host, port, use_native_wisp, wisp_url,
            &wisp_open, &wisp_send, &wisp_recv, &wisp_close,
        ).await?;

        t_stream = js_sys::Date::now();
        debug!("[fetch] stream opened to {}:{} ({:.0}ms)", host, port, t_stream - t_start);

        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| format!("invalid hostname '{}': {}", host, e))?;

        let tls_config = custom_tls.clone().unwrap_or_else(|| TLS_CONFIG.with(|c| c.clone()));
        let connector = tokio_rustls::TlsConnector::from(tls_config);
        let tls_stream = connector
            .connect(server_name, wisp)
            .await
            .map_err(|e| {
                error!("[tls] {} handshake failed: {}", host, e);
                format!("TLS error: {}", e)
            })?;

        t_tls = js_sys::Date::now();
        info!("[tls] handshake complete → {} ({:.0}ms)", host, t_tls - t_stream);

        // Certificate pinning verification
        if let Some(ref pj) = pins_json {
            verify_pins(&tls_stream, &host, pj)?;
        }

        let alpn = tls_stream.get_ref().1.alpn_protocol();
        let is_h2 = alpn.map_or(false, |p| p == b"h2");

        if is_h2 {
            debug!("[http] using HTTP/2 (ALPN negotiated h2)");
        } else {
            debug!("[http] using HTTP/1.1");
        }

        let io = TokioIo::new(tls_stream);

        sender = Some(if is_h2 {
            let (s, conn) = hyper::client::conn::http2::handshake(WasmExecutor, io)
                .await
                .map_err(|e| format!("HTTP/2 handshake failed: {}", e))?;
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = conn.await {
                    warn!("[fetch] HTTP/2 connection error: {}", e);
                }
            });
            PooledSender::H2(s)
        } else {
            let (s, conn) = hyper::client::conn::http1::handshake(io)
                .await
                .map_err(|e| format!("HTTP handshake failed: {}", e))?;
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = conn.await {
                    warn!("[fetch] connection error: {}", e);
                }
            });
            PooledSender::H1(s)
        });
    }

    let mut sender = sender.expect("sender must be set");

    // Parse user headers
    let user_headers = parse_headers_json(&headers_json)?;

    // Build request
    let http_method = method
        .parse::<hyper::Method>()
        .map_err(|e| format!("invalid method '{}': {}", method, e))?;

    let body_bytes = body.unwrap_or_default();
    let mut builder = hyper::Request::builder()
        .method(http_method)
        .uri(&path)
        .header("Host", &host);

    for (key, value) in &user_headers {
        if !key.eq_ignore_ascii_case("host") {
            builder = builder.header(key.as_str(), value.as_str());
        }
    }

    // Inject cookies from jar if enabled
    if use_cookies {
        let cookie_header = COOKIE_JAR.with(|jar| {
            let jar = jar.borrow();
            let request_url = url::Url::parse(&url).ok();
            if let Some(ref u) = request_url {
                let cookies: Vec<String> = jar.matches(u)
                    .iter()
                    .map(|c| format!("{}={}", c.name(), c.value()))
                    .collect();
                if cookies.is_empty() { None } else { Some(cookies.join("; ")) }
            } else {
                None
            }
        });
        if let Some(cv) = cookie_header {
            debug!("[cookie] sending: {}", cv);
            builder = builder.header("Cookie", cv);
        }
    }

    // Auto-add Accept-Encoding if not set by caller
    let has_ae = user_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"));
    if !has_ae {
        builder = builder.header("Accept-Encoding", "gzip, deflate, br");
    }

    // Add Content-Length for non-empty bodies if not already set
    if !body_bytes.is_empty() {
        let has_cl = user_headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
        if !has_cl {
            builder = builder.header("Content-Length", body_bytes.len().to_string());
        }
    }

    let body_len = body_bytes.len();
    debug!("[http] → {} {} ({} bytes body)", method, path, body_len);

    let request = builder
        .body(Full::new(Bytes::from(body_bytes)))
        .map_err(|e| format!("failed to build request: {}", e))?;

    let t_req = js_sys::Date::now();

    let response = match &mut sender {
        PooledSender::H1(ref mut s) => s.send_request(request).await,
        PooledSender::H2(ref mut s) => s.send_request(request).await,
    }
        .map_err(|e| {
            error!("[fetch] {} {} failed: {}", method, url, e);
            format!("request failed: {}", e)
        })?;

    let t_first_byte = js_sys::Date::now();
    let status = response.status().as_u16();
    debug!("[http] ← {} ({:.0}ms to first byte)", status, t_first_byte - t_req);

    // Extract headers
    let headers_obj = js_sys::Object::new();
    for (name, value) in response.headers() {
        let val_str = value.to_str().unwrap_or("");
        js_sys::Reflect::set(
            &headers_obj,
            &JsValue::from_str(name.as_str()),
            &JsValue::from_str(val_str),
        )
        .map_err(|_| "failed to set header")?;
    }

    // Store Set-Cookie headers in jar if cookies enabled
    if use_cookies {
        if let Ok(request_url) = url::Url::parse(&url) {
            COOKIE_JAR.with(|jar| {
                let mut jar = jar.borrow_mut();
                for value in response.headers().get_all("set-cookie") {
                    if let Ok(val_str) = value.to_str() {
                        let raw = cookie::Cookie::parse(val_str.to_string());
                        if let Ok(c) = raw {
                            debug!("[cookie] set {} for {}", c.name(), host);
                            let _ = jar.insert_raw(&c, &request_url);
                        }
                    }
                }
            });
        }
    }

    // Collect body
    let raw_body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("failed to read body: {}", e))?
        .to_bytes();

    // Decompress if Content-Encoding is set
    let content_encoding = headers_obj.clone();
    let ce = js_sys::Reflect::get(&content_encoding, &"content-encoding".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();

    let resp_body: Bytes = match ce.as_str() {
        "gzip" => {
            use std::io::Read;
            let mut decoder = flate2::read::GzDecoder::new(&raw_body[..]);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)
                .map_err(|e| format!("gzip decompression failed: {}", e))?;
            debug!("[decompress] gzip {} → {} bytes", raw_body.len(), decompressed.len());
            Bytes::from(decompressed)
        }
        "deflate" => {
            use std::io::Read;
            // Try zlib first (deflate with zlib header — what most servers send),
            // fall back to raw deflate.
            let mut decompressed = Vec::new();
            let result = {
                let mut decoder = flate2::read::ZlibDecoder::new(&raw_body[..]);
                decoder.read_to_end(&mut decompressed)
            };
            if result.is_err() {
                decompressed.clear();
                let mut decoder = flate2::read::DeflateDecoder::new(&raw_body[..]);
                decoder.read_to_end(&mut decompressed)
                    .map_err(|e| format!("deflate decompression failed: {}", e))?;
            }
            debug!("[decompress] deflate {} → {} bytes", raw_body.len(), decompressed.len());
            Bytes::from(decompressed)
        }
        "br" => {
            let mut decompressed = Vec::new();
            brotli_decompressor::BrotliDecompress(&mut &raw_body[..], &mut decompressed)
                .map_err(|e| format!("brotli decompression failed: {}", e))?;
            debug!("[decompress] brotli {} → {} bytes", raw_body.len(), decompressed.len());
            Bytes::from(decompressed)
        }
        _ => raw_body,
    };

    let t_done = js_sys::Date::now();
    let resp_body_len = resp_body.len();

    info!("[fetch] {} {} → {} ({} bytes, {:.0}ms total)", method, url, status, resp_body_len, t_done - t_start);

    // Build timing object
    let timing = js_sys::Object::new();
    js_sys::Reflect::set(&timing, &"streamOpenMs".into(), &JsValue::from_f64(t_stream - t_start)).ok();
    js_sys::Reflect::set(&timing, &"tlsHandshakeMs".into(), &JsValue::from_f64(t_tls - t_stream)).ok();
    js_sys::Reflect::set(&timing, &"requestSendMs".into(), &JsValue::from_f64(t_req - t_tls)).ok();
    js_sys::Reflect::set(&timing, &"firstByteMs".into(), &JsValue::from_f64(t_first_byte - t_req)).ok();
    js_sys::Reflect::set(&timing, &"bodyDownloadMs".into(), &JsValue::from_f64(t_done - t_first_byte)).ok();
    js_sys::Reflect::set(&timing, &"totalMs".into(), &JsValue::from_f64(t_done - t_start)).ok();

    // Build JS response object
    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &"status".into(), &JsValue::from_f64(status as f64))
        .map_err(|_| "failed to set status")?;
    js_sys::Reflect::set(&obj, &"headers".into(), &headers_obj)
        .map_err(|_| "failed to set headers")?;
    js_sys::Reflect::set(&obj, &"timing".into(), &timing)
        .map_err(|_| "failed to set timing")?;

    let body_arr = js_sys::Uint8Array::from(resp_body.as_ref());
    js_sys::Reflect::set(&obj, &"body".into(), &body_arr)
        .map_err(|_| "failed to set body")?;

    // Return sender to pool for reuse
    CONN_POOL.with(|p| {
        p.borrow_mut().insert(pool_key, PooledConnection {
            sender,
            idle_since: js_sys::Date::now(),
        });
    });

    Ok(obj.into())
}

// ─── Streaming Fetch ─────────────────────────────────────────────

#[wasm_bindgen]
pub async fn atua_fetch_streaming(
    url: String,
    method: String,
    headers_json: String,
    body: Option<Vec<u8>>,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    on_chunk: js_sys::Function,
    timeout_ms: Option<u32>,
    max_redirects: Option<u8>,
    use_cookies: Option<bool>,
    pins_json: Option<String>,
    custom_ca_pem: Option<String>,
    tls_config_json: Option<String>,
    use_native_wisp: Option<bool>,
    wisp_url: Option<String>,
) -> Result<JsValue, JsValue> {
    let native = use_native_wisp.unwrap_or(false);
    let custom_tls = build_custom_tls_config(&custom_ca_pem, &tls_config_json)?;
    let fut = atua_fetch_streaming_inner(
        url.clone(), method.clone(), headers_json, body,
        wisp_send, wisp_recv, wisp_open, wisp_close,
        on_chunk, use_cookies.unwrap_or(false), &pins_json, &custom_tls,
        native, &wisp_url,
    );

    let result = if let Some(ms) = timeout_ms {
        match wasmtimer::tokio::timeout(
            std::time::Duration::from_millis(ms as u64),
            fut,
        ).await {
            Ok(inner) => inner,
            Err(_) => Err(format!("timeout after {}ms", ms)),
        }
    } else {
        fut.await
    };

    result.map_err(|e| JsValue::from_str(&e))
}

async fn atua_fetch_streaming_inner(
    url: String,
    method: String,
    headers_json: String,
    body: Option<Vec<u8>>,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    on_chunk: js_sys::Function,
    use_cookies: bool,
    pins_json: &Option<String>,
    custom_tls: &Option<Arc<rustls::ClientConfig>>,
    use_native_wisp: bool,
    wisp_url: &Option<String>,
) -> Result<JsValue, String> {
    let (host, port, path) = parse_url(&url)?;

    info!("[fetch-stream] {} {}", method, url);

    // Open connection (skip pool for streaming — we don't return the sender)
    let wisp = create_wisp_stream(
        &host, port, use_native_wisp, wisp_url,
        &wisp_open, &wisp_send, &wisp_recv, &wisp_close,
    ).await?;

    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|e| format!("invalid hostname: {}", e))?;
    let tls_config = custom_tls.clone().unwrap_or_else(|| TLS_CONFIG.with(|c| c.clone()));
    let connector = tokio_rustls::TlsConnector::from(tls_config);
    let tls_stream = connector.connect(server_name, wisp).await
        .map_err(|e| format!("TLS error: {}", e))?;

    // Certificate pinning verification
    if let Some(ref pj) = pins_json {
        verify_pins(&tls_stream, &host, pj)?;
    }

    let alpn = tls_stream.get_ref().1.alpn_protocol();
    let is_h2 = alpn.map_or(false, |p| p == b"h2");
    let io = TokioIo::new(tls_stream);

    let mut sender: PooledSender = if is_h2 {
        let (s, conn) = hyper::client::conn::http2::handshake(WasmExecutor, io).await
            .map_err(|e| format!("HTTP/2 handshake failed: {}", e))?;
        wasm_bindgen_futures::spawn_local(async move { let _ = conn.await; });
        PooledSender::H2(s)
    } else {
        let (s, conn) = hyper::client::conn::http1::handshake(io).await
            .map_err(|e| format!("HTTP handshake failed: {}", e))?;
        wasm_bindgen_futures::spawn_local(async move { let _ = conn.await; });
        PooledSender::H1(s)
    };

    let user_headers = parse_headers_json(&headers_json)?;
    let http_method = method.parse::<hyper::Method>()
        .map_err(|e| format!("invalid method: {}", e))?;

    let body_bytes = body.unwrap_or_default();
    let mut builder = hyper::Request::builder()
        .method(http_method)
        .uri(&path)
        .header("Host", &host);

    for (key, value) in &user_headers {
        if !key.eq_ignore_ascii_case("host") {
            builder = builder.header(key.as_str(), value.as_str());
        }
    }

    // Inject cookies from jar if enabled
    if use_cookies {
        let cookie_header = COOKIE_JAR.with(|jar| {
            let jar = jar.borrow();
            let request_url = url::Url::parse(&url).ok();
            if let Some(ref u) = request_url {
                let cookies: Vec<String> = jar.matches(u)
                    .iter()
                    .map(|c| format!("{}={}", c.name(), c.value()))
                    .collect();
                if cookies.is_empty() { None } else { Some(cookies.join("; ")) }
            } else {
                None
            }
        });
        if let Some(cv) = cookie_header {
            builder = builder.header("Cookie", cv);
        }
    }

    // Streaming: do NOT auto-add Accept-Encoding (would get compressed chunks)
    // Unless caller explicitly set it
    if !body_bytes.is_empty() {
        let has_cl = user_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
        if !has_cl {
            builder = builder.header("Content-Length", body_bytes.len().to_string());
        }
    }

    let request = builder
        .body(Full::new(Bytes::from(body_bytes)))
        .map_err(|e| format!("failed to build request: {}", e))?;

    let response = match &mut sender {
        PooledSender::H1(s) => s.send_request(request).await,
        PooledSender::H2(s) => s.send_request(request).await,
    }.map_err(|e| format!("request failed: {}", e))?;

    let status = response.status().as_u16();

    // Extract headers
    let headers_obj = js_sys::Object::new();
    for (name, value) in response.headers() {
        let val_str = value.to_str().unwrap_or("");
        js_sys::Reflect::set(&headers_obj, &JsValue::from_str(name.as_str()), &JsValue::from_str(val_str)).ok();
    }

    // Stream body frames via on_chunk callback
    use http_body::Body as _;
    let mut body = response.into_body();
    loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    let arr = js_sys::Uint8Array::from(data.as_ref());
                    call_js(&on_chunk, &[arr.into()]).await?;
                }
            }
            Some(Err(e)) => return Err(format!("stream error: {}", e)),
            None => break,
        }
    }

    info!("[fetch-stream] {} {} → {} complete", method, url, status);

    // Return { status, headers } — body was delivered via callbacks
    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &"status".into(), &JsValue::from_f64(status as f64)).ok();
    js_sys::Reflect::set(&obj, &"headers".into(), &headers_obj).ok();
    Ok(obj.into())
}

// ─── Raw TCP/TLS Connect ─────────────────────────────────────────

#[wasm_bindgen]
pub async fn atua_connect(
    host: String,
    port: u16,
    use_tls: bool,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    use_native_wisp: Option<bool>,
    wisp_url: Option<String>,
) -> Result<JsValue, JsValue> {
    let native = use_native_wisp.unwrap_or(false);
    atua_connect_inner(host, port, use_tls, wisp_send, wisp_recv, wisp_open, wisp_close, native, wisp_url)
        .await
        .map_err(|e| JsValue::from_str(&e))
}

async fn atua_connect_inner(
    host: String,
    port: u16,
    use_tls: bool,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    use_native_wisp: bool,
    wisp_url: Option<String>,
) -> Result<JsValue, String> {
    let wisp = create_wisp_stream(
        &host, port, use_native_wisp, &wisp_url,
        &wisp_open, &wisp_send, &wisp_recv, &wisp_close,
    ).await?;

    let key = next_stream_key();

    if use_tls {
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| format!("invalid hostname '{}': {}", host, e))?;

        // Use raw TLS config (no ALPN) for raw streams — caller manages protocol
        let tls_config = TLS_CONFIG_RAW.with(|c| c.clone());
        let connector = tokio_rustls::TlsConnector::from(tls_config);
        let tls_stream = connector
            .connect(server_name, wisp)
            .await
            .map_err(|e| format!("TLS error: {}", e))?;

        STREAMS.with(|s| s.borrow_mut().insert(key.clone(), StoredStream::Tls(tls_stream)));
    } else {
        STREAMS.with(|s| s.borrow_mut().insert(key.clone(), StoredStream::Raw(wisp)));
    }

    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &"streamId".into(), &JsValue::from_str(&key))
        .map_err(|_| "failed to set streamId")?;
    js_sys::Reflect::set(&obj, &"tls".into(), &JsValue::from_bool(use_tls))
        .map_err(|_| "failed to set tls")?;

    Ok(obj.into())
}

// ─── Stream send/recv/close (for atua_connect streams) ───────────

#[wasm_bindgen]
pub async fn atua_stream_send(stream_key: String, data: Vec<u8>) -> Result<(), JsValue> {
    let result: Result<(), String> = async {
        let mut stream_opt = STREAMS.with(|s| s.borrow_mut().remove(&stream_key));
        let stream = stream_opt.as_mut().ok_or("stream not found")?;

        match stream {
            StoredStream::Tls(tls) => {
                tls.write_all(&data)
                    .await
                    .map_err(|e| format!("TLS write failed: {}", e))?;
                tls.flush().await.map_err(|e| format!("TLS flush failed: {}", e))?;
            }
            StoredStream::Raw(raw) => {
                raw.write_all(&data)
                    .await
                    .map_err(|e| format!("write failed: {}", e))?;
            }
        }

        // Put stream back
        STREAMS.with(|s| s.borrow_mut().insert(stream_key.clone(), stream_opt.take().unwrap()));
        Ok(())
    }
    .await;

    // If we took the stream out but errored, try to put it back
    result.map_err(|e| JsValue::from_str(&e))
}

#[wasm_bindgen]
pub async fn atua_stream_recv(stream_key: String) -> Result<JsValue, JsValue> {
    let result: Result<JsValue, String> = async {
        let mut stream_opt = STREAMS.with(|s| s.borrow_mut().remove(&stream_key));
        let stream = stream_opt.as_mut().ok_or("stream not found")?;

        let mut buf = vec![0u8; 16384];
        let n = match stream {
            StoredStream::Tls(tls) => tls
                .read(&mut buf)
                .await
                .map_err(|e| format!("TLS read failed: {}", e))?,
            StoredStream::Raw(raw) => raw
                .read(&mut buf)
                .await
                .map_err(|e| format!("read failed: {}", e))?,
        };

        buf.truncate(n);

        // Put stream back
        STREAMS.with(|s| s.borrow_mut().insert(stream_key.clone(), stream_opt.take().unwrap()));

        let arr = js_sys::Uint8Array::from(buf.as_slice());
        Ok(arr.into())
    }
    .await;

    result.map_err(|e| JsValue::from_str(&e))
}

#[wasm_bindgen]
pub async fn atua_stream_close(stream_key: String) -> Result<(), JsValue> {
    let stream_opt = STREAMS.with(|s| s.borrow_mut().remove(&stream_key));
    if let Some(mut stream) = stream_opt {
        let _ = match &mut stream {
            StoredStream::Tls(tls) => tls.shutdown().await,
            StoredStream::Raw(raw) => raw.shutdown().await,
        };
    }
    Ok(())
}

// ─── WebSocket ───────────────────────────────────────────────────

type WsStream = tokio_rustls::client::TlsStream<WispStream>;

thread_local! {
    static WS_STREAMS: RefCell<HashMap<String, WsStream>> = RefCell::new(HashMap::new());
}

#[wasm_bindgen]
pub async fn atua_websocket(
    url: String,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    use_native_wisp: Option<bool>,
    wisp_url: Option<String>,
) -> Result<JsValue, JsValue> {
    let native = use_native_wisp.unwrap_or(false);
    atua_websocket_inner(url, wisp_send, wisp_recv, wisp_open, wisp_close, native, wisp_url)
        .await
        .map_err(|e| JsValue::from_str(&e))
}

async fn atua_websocket_inner(
    url: String,
    wisp_send: Option<js_sys::Function>,
    wisp_recv: Option<js_sys::Function>,
    wisp_open: Option<js_sys::Function>,
    wisp_close: Option<js_sys::Function>,
    use_native_wisp: bool,
    wisp_url: Option<String>,
) -> Result<JsValue, String> {
    // Parse wss:// URL
    let parsed = url::Url::parse(url.trim())
        .map_err(|e| format!("invalid URL: {}", e))?;
    if parsed.scheme() != "wss" {
        return Err(format!("only wss:// URLs supported, got: {}", parsed.scheme()));
    }
    let host = parsed.host_str().ok_or("no host")?.to_string();
    let port = parsed.port().unwrap_or(443);
    let path = {
        let p = &parsed[url::Position::BeforePath..];
        if p.is_empty() { "/".to_string() } else { p.to_string() }
    };

    info!("[ws] connecting to {}:{}{}", host, port, path);

    // Open Wisp stream
    let wisp = create_wisp_stream(
        &host, port, use_native_wisp, &wisp_url,
        &wisp_open, &wisp_send, &wisp_recv, &wisp_close,
    ).await?;

    // TLS handshake (no ALPN for WebSocket — raw TLS)
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|e| format!("invalid hostname: {}", e))?;
    let tls_config = TLS_CONFIG_RAW.with(|c| c.clone());
    let connector = tokio_rustls::TlsConnector::from(tls_config);
    let mut tls_stream = connector
        .connect(server_name, wisp)
        .await
        .map_err(|e| format!("TLS error: {}", e))?;

    // WebSocket upgrade handshake
    websocket::client_handshake(&mut tls_stream, &host, &path).await?;

    info!("[ws] connected to {}", host);

    // Store and return key
    let key = next_stream_key();
    WS_STREAMS.with(|s| s.borrow_mut().insert(key.clone(), tls_stream));

    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &"streamId".into(), &JsValue::from_str(&key))
        .map_err(|_| "failed to set streamId")?;
    Ok(obj.into())
}

#[wasm_bindgen]
pub async fn atua_ws_send(key: String, data: String) -> Result<(), JsValue> {
    let result: Result<(), String> = async {
        let mut stream = WS_STREAMS.with(|s| s.borrow_mut().remove(&key))
            .ok_or("ws stream not found")?;
        websocket::send_text(&mut stream, &data).await?;
        WS_STREAMS.with(|s| s.borrow_mut().insert(key.clone(), stream));
        Ok(())
    }.await;
    result.map_err(|e| JsValue::from_str(&e))
}

#[wasm_bindgen]
pub async fn atua_ws_recv(key: String) -> Result<JsValue, JsValue> {
    let result: Result<JsValue, String> = async {
        let mut stream = WS_STREAMS.with(|s| s.borrow_mut().remove(&key))
            .ok_or("ws stream not found")?;
        let msg = websocket::recv_message(&mut stream).await?;
        WS_STREAMS.with(|s| s.borrow_mut().insert(key.clone(), stream));
        match msg {
            Some(text) => Ok(JsValue::from_str(&text)),
            None => Ok(JsValue::NULL),
        }
    }.await;
    result.map_err(|e| JsValue::from_str(&e))
}

#[wasm_bindgen]
pub async fn atua_ws_close(key: String) -> Result<(), JsValue> {
    let result: Result<(), String> = async {
        let mut stream = WS_STREAMS.with(|s| s.borrow_mut().remove(&key));
        if let Some(ref mut s) = stream {
            let _ = websocket::send_close(s).await;
        }
        Ok(())
    }.await;
    result.map_err(|e| JsValue::from_str(&e))
}

// ─── Diagnostics ─────────────────────────────────────────────────

#[wasm_bindgen]
pub fn atua_diagnostics() -> JsValue {
    let wisp_streams: usize = wisp::WISP_CLIENTS.with(|clients| {
        clients.borrow().values().map(|c| c.stream_count()).sum()
    });
    let wisp_connections: usize = wisp::WISP_CLIENTS.with(|clients| {
        clients.borrow().len()
    });
    let pooled_connections: usize = CONN_POOL.with(|p| p.borrow().len());
    let stored_streams: usize = STREAMS.with(|s| s.borrow().len());
    let ws_streams: usize = WS_STREAMS.with(|s| s.borrow().len());

    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &"wispStreams".into(), &JsValue::from_f64(wisp_streams as f64)).ok();
    js_sys::Reflect::set(&obj, &"wispConnections".into(), &JsValue::from_f64(wisp_connections as f64)).ok();
    js_sys::Reflect::set(&obj, &"pooledConnections".into(), &JsValue::from_f64(pooled_connections as f64)).ok();
    js_sys::Reflect::set(&obj, &"storedStreams".into(), &JsValue::from_f64(stored_streams as f64)).ok();
    js_sys::Reflect::set(&obj, &"wsStreams".into(), &JsValue::from_f64(ws_streams as f64)).ok();
    obj.into()
}

// ─── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_url() {
        let (host, port, path) = parse_url("https://api.anthropic.com/v1/messages").unwrap();
        assert_eq!(host, "api.anthropic.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/v1/messages");
    }

    #[test]
    fn parse_url_defaults_port_443() {
        let (_, port, _) = parse_url("https://example.com/test").unwrap();
        assert_eq!(port, 443);
    }

    #[test]
    fn parse_url_with_query_string() {
        let (_, _, path) = parse_url("https://httpbin.org/get?foo=bar").unwrap();
        assert_eq!(path, "/get?foo=bar");
    }

    #[test]
    fn rejects_non_https() {
        assert!(parse_url("http://example.com").is_err());
    }

    #[test]
    fn parse_empty_headers() {
        assert!(parse_headers_json("{}").unwrap().is_empty());
    }

    #[test]
    fn parse_simple_headers() {
        let h = parse_headers_json(r#"{"Content-Type": "application/json"}"#).unwrap();
        assert!(h.iter().any(|(k, v)| k == "Content-Type" && v == "application/json"));
    }

    #[test]
    fn parse_headers_rejects_invalid() {
        assert!(parse_headers_json("not json").is_err());
    }
}
