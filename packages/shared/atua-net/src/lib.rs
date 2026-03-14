mod http;
mod tls;

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Parse a URL into (host, port, path). Only HTTPS is supported.
fn parse_url(url: &str) -> Result<(String, u16, String), String> {
    let url = url.trim();
    let without_scheme = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("only HTTPS URLs are supported, got: {}", url))?;

    let (host_port, path) = match without_scheme.find('/') {
        Some(i) => (&without_scheme[..i], &without_scheme[i..]),
        None => (without_scheme, "/"),
    };

    let (host, port) = match host_port.rfind(':') {
        Some(i) => {
            let port_str = &host_port[i + 1..];
            let port = port_str
                .parse::<u16>()
                .map_err(|_| format!("invalid port: {}", port_str))?;
            (&host_port[..i], port)
        }
        None => (host_port, 443),
    };

    if host.is_empty() {
        return Err("empty hostname".into());
    }

    Ok((host.to_string(), port, path.to_string()))
}

/// Parse JSON headers string into key-value pairs.
fn parse_headers_json(json: &str) -> Result<Vec<(String, String)>, String> {
    if json.is_empty() || json == "{}" {
        return Ok(Vec::new());
    }

    // Simple JSON object parser for {"key": "value", ...}
    let json = json.trim();
    if !json.starts_with('{') || !json.ends_with('}') {
        return Err("headers must be a JSON object".into());
    }

    let inner = &json[1..json.len() - 1];
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut headers = Vec::new();
    let mut chars = inner.chars().peekable();

    loop {
        // Skip whitespace
        while chars.peek().map_or(false, |c| c.is_whitespace() || *c == ',') {
            chars.next();
        }

        if chars.peek().is_none() {
            break;
        }

        // Parse key
        let key = parse_json_string(&mut chars)?;

        // Skip colon
        while chars.peek().map_or(false, |c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next() != Some(':') {
            return Err("expected ':' in headers JSON".into());
        }

        // Skip whitespace
        while chars.peek().map_or(false, |c| c.is_whitespace()) {
            chars.next();
        }

        // Parse value
        let value = parse_json_string(&mut chars)?;

        headers.push((key, value));
    }

    Ok(headers)
}

fn parse_json_string(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<String, String> {
    if chars.next() != Some('"') {
        return Err("expected '\"' in JSON string".into());
    }

    let mut result = String::new();
    loop {
        match chars.next() {
            Some('\\') => match chars.next() {
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some('/') => result.push('/'),
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some(c) => {
                    result.push('\\');
                    result.push(c);
                }
                None => return Err("unterminated escape in JSON string".into()),
            },
            Some('"') => return Ok(result),
            Some(c) => result.push(c),
            None => return Err("unterminated JSON string".into()),
        }
    }
}

/// Call a JS function that returns a Promise and await the result.
async fn call_js_async(func: &js_sys::Function, args: &[JsValue]) -> Result<JsValue, String> {
    let this = JsValue::NULL;
    let result = match args.len() {
        0 => func.call0(&this),
        1 => func.call1(&this, &args[0]),
        2 => func.call2(&this, &args[0], &args[1]),
        3 => func.call3(&this, &args[0], &args[1], &args[2]),
        _ => {
            let js_args = js_sys::Array::new();
            for arg in args {
                js_args.push(arg);
            }
            func.apply(&this, &js_args)
        }
    }
    .map_err(|e| format!("JS call failed: {:?}", e))?;

    if result.is_instance_of::<js_sys::Promise>() {
        let promise = js_sys::Promise::from(result);
        JsFuture::from(promise)
            .await
            .map_err(|e| format!("JS promise rejected: {:?}", e))
    } else {
        Ok(result)
    }
}

/// Perform the TLS handshake, pumping bytes via Wisp callbacks.
async fn do_handshake(
    bridge: &mut tls::TlsBridge,
    stream_id: &JsValue,
    wisp_send: &js_sys::Function,
    wisp_recv: &js_sys::Function,
) -> Result<(), String> {
    while bridge.is_handshaking() {
        // Send any pending ciphertext (ClientHello, etc.)
        if bridge.wants_write() {
            let ct = bridge.take_ciphertext();
            if !ct.is_empty() {
                let arr = js_sys::Uint8Array::from(ct.as_slice());
                call_js_async(wisp_send, &[stream_id.clone(), arr.into()]).await?;
            }
        }

        if !bridge.is_handshaking() {
            break;
        }

        // Receive server response
        let recv_result =
            call_js_async(wisp_recv, &[stream_id.clone()]).await?;
        let data = js_sys::Uint8Array::new(&recv_result);
        let mut buf = vec![0u8; data.length() as usize];
        data.copy_to(&mut buf);

        if buf.is_empty() {
            return Err("connection closed during TLS handshake".into());
        }

        bridge.feed_ciphertext(&buf)?;
    }

    // Flush any remaining handshake ciphertext
    if bridge.wants_write() {
        let ct = bridge.take_ciphertext();
        if !ct.is_empty() {
            let arr = js_sys::Uint8Array::from(ct.as_slice());
            call_js_async(wisp_send, &[stream_id.clone(), arr.into()]).await?;
        }
    }

    Ok(())
}

/// HTTPS fetch through Wisp relay with end-to-end TLS.
#[wasm_bindgen]
pub async fn atua_fetch(
    url: String,
    method: String,
    headers_json: String,
    body: Option<Vec<u8>>,
    wisp_send: js_sys::Function,
    wisp_recv: js_sys::Function,
    wisp_open: js_sys::Function,
    wisp_close: js_sys::Function,
) -> Result<JsValue, JsValue> {
    let result = atua_fetch_inner(
        url,
        method,
        headers_json,
        body,
        wisp_send,
        wisp_recv,
        wisp_open,
        wisp_close,
    )
    .await;

    result.map_err(|e| JsValue::from_str(&e))
}

async fn atua_fetch_inner(
    url: String,
    method: String,
    headers_json: String,
    body: Option<Vec<u8>>,
    wisp_send: js_sys::Function,
    wisp_recv: js_sys::Function,
    wisp_open: js_sys::Function,
    wisp_close: js_sys::Function,
) -> Result<JsValue, String> {
    let (host, port, path) = parse_url(&url)?;

    // Open Wisp stream
    let stream_id = call_js_async(
        &wisp_open,
        &[JsValue::from_str(&host), JsValue::from_f64(port as f64)],
    )
    .await?;

    // Create TLS bridge and perform handshake
    let mut bridge = tls::TlsBridge::new(&host)?;

    let result: Result<http::Response, String> = async {
        do_handshake(&mut bridge, &stream_id, &wisp_send, &wisp_recv).await?;

        // Build and send HTTP request
        let headers = parse_headers_json(&headers_json)?;
        let request =
            http::serialize_request(&method, &path, &host, &headers, body.as_deref());

        bridge.feed_plaintext(&request)?;

        let ct = bridge.take_ciphertext();
        if !ct.is_empty() {
            let arr = js_sys::Uint8Array::from(ct.as_slice());
            call_js_async(&wisp_send, &[stream_id.clone(), arr.into()]).await?;
        }

        // Read response
        let mut parser = http::ResponseParser::new();

        loop {
            let recv_result =
                call_js_async(&wisp_recv, &[stream_id.clone()]).await?;
            let data = js_sys::Uint8Array::new(&recv_result);
            let mut buf = vec![0u8; data.length() as usize];
            data.copy_to(&mut buf);

            if buf.is_empty() {
                // Connection closed
                if parser.headers_done()
                    && !parser.has_content_length()
                    && !parser.is_chunked()
                {
                    let resp = parser.finish_no_length();
                    return Ok(resp);
                }
                return Err("connection closed before response complete".into());
            }

            bridge.feed_ciphertext(&buf)?;
            let plaintext = bridge.take_plaintext();

            if plaintext.is_empty() {
                continue;
            }

            match parser.feed(&plaintext)? {
                http::FeedResult::Complete(resp) => return Ok(resp),
                http::FeedResult::NeedMore | http::FeedResult::Chunk(_) => continue,
            }
        }
    }
    .await;

    // Always close the Wisp stream
    let _ = call_js_async(&wisp_close, &[stream_id]).await;

    let resp = result?;

    // Build JS response object: { status, headers, body }
    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &"status".into(), &JsValue::from_f64(resp.status as f64))
        .map_err(|_| "failed to set status")?;

    let headers_obj = js_sys::Object::new();
    for (key, value) in &resp.headers {
        js_sys::Reflect::set(&headers_obj, &JsValue::from_str(key), &JsValue::from_str(value))
            .map_err(|_| "failed to set header")?;
    }
    js_sys::Reflect::set(&obj, &"headers".into(), &headers_obj)
        .map_err(|_| "failed to set headers")?;

    let body_arr = js_sys::Uint8Array::from(resp.body.as_slice());
    js_sys::Reflect::set(&obj, &"body".into(), &body_arr)
        .map_err(|_| "failed to set body")?;

    Ok(obj.into())
}

/// Open a raw TCP/TLS stream through Wisp relay.
/// Returns a handle object with send(data), recv(), and close() methods.
#[wasm_bindgen]
pub async fn atua_connect(
    host: String,
    port: u16,
    use_tls: bool,
    wisp_send: js_sys::Function,
    wisp_recv: js_sys::Function,
    wisp_open: js_sys::Function,
    wisp_close: js_sys::Function,
) -> Result<JsValue, JsValue> {
    let result = atua_connect_inner(host, port, use_tls, wisp_send, wisp_recv, wisp_open, wisp_close).await;
    result.map_err(|e| JsValue::from_str(&e))
}

async fn atua_connect_inner(
    host: String,
    port: u16,
    use_tls: bool,
    wisp_send: js_sys::Function,
    wisp_recv: js_sys::Function,
    wisp_open: js_sys::Function,
    _wisp_close: js_sys::Function,
) -> Result<JsValue, String> {
    // Open Wisp stream
    let stream_id = call_js_async(
        &wisp_open,
        &[JsValue::from_str(&host), JsValue::from_f64(port as f64)],
    )
    .await?;

    if use_tls {
        let mut bridge = tls::TlsBridge::new(&host)?;
        do_handshake(&mut bridge, &stream_id, &wisp_send, &wisp_recv).await?;
    }

    // Return stream handle info for JS wrapper to manage
    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &"streamId".into(), &stream_id)
        .map_err(|_| "failed to set streamId")?;
    js_sys::Reflect::set(&obj, &"tls".into(), &JsValue::from_bool(use_tls))
        .map_err(|_| "failed to set tls")?;

    Ok(obj.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── URL Parsing ───────────────────────────────────────────

    #[test]
    fn parse_https_url() {
        let (host, port, path) = parse_url("https://api.anthropic.com/v1/messages").unwrap();
        assert_eq!(host, "api.anthropic.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/v1/messages");
    }

    #[test]
    fn parse_url_with_port() {
        let (host, port, path) = parse_url("https://localhost:8443/test").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 8443);
        assert_eq!(path, "/test");
    }

    #[test]
    fn parse_url_no_path() {
        let (host, port, path) = parse_url("https://example.com").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_url_with_query_string() {
        let (host, port, path) =
            parse_url("https://httpbin.org/get?foo=bar&baz=qux").unwrap();
        assert_eq!(host, "httpbin.org");
        assert_eq!(port, 443);
        assert_eq!(path, "/get?foo=bar&baz=qux");
    }

    #[test]
    fn parse_url_with_fragment() {
        let (host, _, path) = parse_url("https://example.com/page#section").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(path, "/page#section");
    }

    #[test]
    fn parse_url_root_path() {
        let (host, _, path) = parse_url("https://example.com/").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_url_deep_path() {
        let (_, _, path) =
            parse_url("https://example.com/a/b/c/d/e/file.json").unwrap();
        assert_eq!(path, "/a/b/c/d/e/file.json");
    }

    #[test]
    fn parse_url_port_443() {
        let (host, port, _) = parse_url("https://example.com:443/").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn parse_url_port_8080() {
        let (host, port, _) = parse_url("https://example.com:8080/api").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 8080);
    }

    #[test]
    fn parse_url_subdomain() {
        let (host, _, _) = parse_url("https://sub.domain.example.com/").unwrap();
        assert_eq!(host, "sub.domain.example.com");
    }

    #[test]
    fn parse_url_with_whitespace() {
        let (host, _, _) = parse_url("  https://example.com/  ").unwrap();
        assert_eq!(host, "example.com");
    }

    #[test]
    fn rejects_http_url() {
        assert!(parse_url("http://example.com").is_err());
    }

    #[test]
    fn rejects_empty_url() {
        assert!(parse_url("").is_err());
    }

    #[test]
    fn rejects_no_scheme() {
        assert!(parse_url("example.com").is_err());
    }

    #[test]
    fn rejects_ftp_url() {
        assert!(parse_url("ftp://example.com").is_err());
    }

    #[test]
    fn rejects_just_scheme() {
        assert!(parse_url("https://").is_err());
    }

    #[test]
    fn rejects_invalid_port() {
        assert!(parse_url("https://example.com:notaport/").is_err());
    }

    #[test]
    fn rejects_port_overflow() {
        assert!(parse_url("https://example.com:99999/").is_err());
    }

    // ─── JSON Header Parsing ───────────────────────────────────

    #[test]
    fn parse_empty_headers() {
        let h = parse_headers_json("{}").unwrap();
        assert!(h.is_empty());
    }

    #[test]
    fn parse_empty_string_headers() {
        let h = parse_headers_json("").unwrap();
        assert!(h.is_empty());
    }

    #[test]
    fn parse_simple_headers() {
        let h = parse_headers_json(r#"{"Content-Type": "application/json", "X-Key": "abc"}"#)
            .unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].0, "Content-Type");
        assert_eq!(h[0].1, "application/json");
        assert_eq!(h[1].0, "X-Key");
        assert_eq!(h[1].1, "abc");
    }

    #[test]
    fn parse_headers_with_escaped_chars() {
        let h = parse_headers_json(r#"{"Key": "value with \"quotes\""}"#).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, r#"value with "quotes""#);
    }

    #[test]
    fn parse_headers_with_special_values() {
        let h = parse_headers_json(r#"{"Authorization": "Bearer sk-ant-api03-abc123"}"#).unwrap();
        assert_eq!(h[0].1, "Bearer sk-ant-api03-abc123");
    }

    #[test]
    fn parse_headers_with_backslash() {
        let h = parse_headers_json(r#"{"Key": "path\\to\\file"}"#).unwrap();
        assert_eq!(h[0].1, r"path\to\file");
    }

    #[test]
    fn parse_headers_single() {
        let h = parse_headers_json(r#"{"Accept": "*/*"}"#).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "Accept");
        assert_eq!(h[0].1, "*/*");
    }

    #[test]
    fn parse_headers_many() {
        let h = parse_headers_json(
            r#"{"A": "1", "B": "2", "C": "3", "D": "4", "E": "5"}"#,
        )
        .unwrap();
        assert_eq!(h.len(), 5);
    }

    #[test]
    fn parse_headers_whitespace_padded() {
        let h = parse_headers_json(r#"  { "Key" : "Value" }  "#).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "Key");
        assert_eq!(h[0].1, "Value");
    }

    #[test]
    fn parse_headers_rejects_invalid_json() {
        assert!(parse_headers_json("not json").is_err());
        assert!(parse_headers_json("[1,2,3]").is_err());
        assert!(parse_headers_json("{invalid}").is_err());
    }

    #[test]
    fn parse_headers_empty_value() {
        let h = parse_headers_json(r#"{"Key": ""}"#).unwrap();
        assert_eq!(h[0].1, "");
    }

    #[test]
    fn parse_headers_with_newline_escape() {
        let h = parse_headers_json(r#"{"Key": "line1\nline2"}"#).unwrap();
        assert_eq!(h[0].1, "line1\nline2");
    }
}
