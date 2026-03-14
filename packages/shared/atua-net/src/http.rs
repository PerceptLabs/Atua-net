/// Serialize an HTTP/1.1 request to bytes.
pub fn serialize_request(
    method: &str,
    path: &str,
    host: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut req = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", method, path, host);

    for (key, value) in headers {
        // Skip Host header since we already added it
        if key.eq_ignore_ascii_case("host") {
            continue;
        }
        req.push_str(&format!("{}: {}\r\n", key, value));
    }

    if let Some(b) = body {
        // Only add Content-Length if not already present
        let has_cl = headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
        if !has_cl {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
    }

    req.push_str("\r\n");

    let mut result = req.into_bytes();
    if let Some(b) = body {
        result.extend_from_slice(b);
    }
    result
}

/// Result of feeding data to the response parser.
#[derive(Debug)]
pub enum FeedResult {
    /// Need more data before we can produce output.
    NeedMore,
    /// A chunk of body data is available (for streaming).
    Chunk(Vec<u8>),
    /// Response is complete.
    Complete(Response),
}

/// A parsed HTTP response.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// State for chunked transfer decoding.
#[derive(Debug)]
enum ChunkState {
    ReadingSize,
    ReadingData { remaining: usize },
    ReadingTrailer,
    Done,
}

/// HTTP/1.1 response parser. Feed it decrypted bytes, get back parsed responses.
pub struct ResponseParser {
    headers_complete: bool,
    status: u16,
    headers: Vec<(String, String)>,
    content_length: Option<usize>,
    is_chunked: bool,
    chunk_state: ChunkState,
    body: Vec<u8>,
    header_buf: Vec<u8>,
    chunk_line_buf: Vec<u8>,
}

impl ResponseParser {
    pub fn new() -> Self {
        Self {
            headers_complete: false,
            status: 0,
            headers: Vec::new(),
            content_length: None,
            is_chunked: false,
            chunk_state: ChunkState::ReadingSize,
            body: Vec::new(),
            header_buf: Vec::new(),
            chunk_line_buf: Vec::new(),
        }
    }

    /// Feed decrypted bytes into the parser.
    pub fn feed(&mut self, data: &[u8]) -> Result<FeedResult, String> {
        if !self.headers_complete {
            self.header_buf.extend_from_slice(data);
            return self.try_parse_headers();
        }

        self.feed_body(data)
    }

    fn try_parse_headers(&mut self) -> Result<FeedResult, String> {
        // Check if we have the complete header block (\r\n\r\n)
        let header_end = self
            .header_buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n");

        let header_end = match header_end {
            Some(pos) => pos,
            None => return Ok(FeedResult::NeedMore),
        };

        let header_bytes = &self.header_buf[..header_end + 4];

        let mut parsed_headers = [httparse::EMPTY_HEADER; 128];
        let mut response = httparse::Response::new(&mut parsed_headers);

        match response.parse(header_bytes) {
            Ok(httparse::Status::Complete(_)) => {}
            Ok(httparse::Status::Partial) => return Ok(FeedResult::NeedMore),
            Err(e) => return Err(format!("HTTP parse error: {}", e)),
        }

        self.status = response.code.unwrap_or(0);
        self.headers = response
            .headers
            .iter()
            .map(|h| {
                (
                    h.name.to_string(),
                    String::from_utf8_lossy(h.value).to_string(),
                )
            })
            .collect();

        // Determine body framing
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("content-length") {
                self.content_length = value.trim().parse::<usize>().ok();
            }
            if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_lowercase().contains("chunked")
            {
                self.is_chunked = true;
            }
        }

        self.headers_complete = true;

        // Body bytes that came after the headers
        let remaining = self.header_buf[header_end + 4..].to_vec();
        self.header_buf.clear();

        // For responses with no body (204, 304, 1xx)
        if self.status == 204 || self.status == 304 || self.status < 200 {
            return Ok(FeedResult::Complete(Response {
                status: self.status,
                headers: self.headers.clone(),
                body: Vec::new(),
            }));
        }

        // If Content-Length is 0
        if self.content_length == Some(0) {
            return Ok(FeedResult::Complete(Response {
                status: self.status,
                headers: self.headers.clone(),
                body: Vec::new(),
            }));
        }

        if !remaining.is_empty() {
            return self.feed_body(&remaining);
        }

        Ok(FeedResult::NeedMore)
    }

    fn feed_body(&mut self, data: &[u8]) -> Result<FeedResult, String> {
        if self.is_chunked {
            self.feed_chunked(data)
        } else if let Some(cl) = self.content_length {
            self.body.extend_from_slice(data);
            if self.body.len() >= cl {
                self.body.truncate(cl);
                Ok(FeedResult::Complete(Response {
                    status: self.status,
                    headers: self.headers.clone(),
                    body: std::mem::take(&mut self.body),
                }))
            } else {
                Ok(FeedResult::Chunk(data.to_vec()))
            }
        } else {
            // No Content-Length, no chunked — read until connection close.
            // We accumulate and return chunks; caller decides when done.
            self.body.extend_from_slice(data);
            Ok(FeedResult::Chunk(data.to_vec()))
        }
    }

    fn feed_chunked(&mut self, data: &[u8]) -> Result<FeedResult, String> {
        let mut pos = 0;

        while pos < data.len() {
            match &self.chunk_state {
                ChunkState::Done => break,
                ChunkState::ReadingSize => {
                    // Accumulate until we find \r\n
                    while pos < data.len() {
                        let b = data[pos];
                        pos += 1;
                        self.chunk_line_buf.push(b);

                        if self.chunk_line_buf.ends_with(b"\r\n") {
                            let line = String::from_utf8_lossy(
                                &self.chunk_line_buf[..self.chunk_line_buf.len() - 2],
                            );
                            // Chunk size may have extensions after ';'
                            let size_str = line.split(';').next().unwrap_or("").trim();
                            let size = usize::from_str_radix(size_str, 16)
                                .map_err(|e| format!("invalid chunk size '{}': {}", size_str, e))?;

                            self.chunk_line_buf.clear();

                            if size == 0 {
                                self.chunk_state = ChunkState::Done;
                            } else {
                                self.chunk_state = ChunkState::ReadingData { remaining: size };
                            }
                            break;
                        }
                    }
                }
                ChunkState::ReadingData { remaining } => {
                    let remaining = *remaining;
                    let available = data.len() - pos;
                    let to_read = remaining.min(available);

                    self.body.extend_from_slice(&data[pos..pos + to_read]);
                    pos += to_read;

                    if to_read < remaining {
                        self.chunk_state = ChunkState::ReadingData {
                            remaining: remaining - to_read,
                        };
                    } else {
                        self.chunk_state = ChunkState::ReadingTrailer;
                    }
                }
                ChunkState::ReadingTrailer => {
                    // Consume \r\n after chunk data
                    while pos < data.len() {
                        let b = data[pos];
                        pos += 1;
                        self.chunk_line_buf.push(b);
                        if self.chunk_line_buf.ends_with(b"\r\n") {
                            self.chunk_line_buf.clear();
                            self.chunk_state = ChunkState::ReadingSize;
                            break;
                        }
                    }
                }
            }
        }

        match &self.chunk_state {
            ChunkState::Done => Ok(FeedResult::Complete(Response {
                status: self.status,
                headers: self.headers.clone(),
                body: std::mem::take(&mut self.body),
            })),
            _ => Ok(FeedResult::NeedMore),
        }
    }

    /// For connection-close framing: finalize with whatever body we have.
    pub fn finish_no_length(&self) -> Response {
        Response {
            status: self.status,
            headers: self.headers.clone(),
            body: self.body.clone(),
        }
    }

    pub fn headers_done(&self) -> bool {
        self.headers_complete
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn has_content_length(&self) -> bool {
        self.content_length.is_some()
    }

    pub fn is_chunked(&self) -> bool {
        self.is_chunked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_get_request() {
        let data = serialize_request(
            "GET",
            "/test",
            "example.com",
            &[("Accept".into(), "text/html".into())],
            None,
        );
        let s = String::from_utf8(data).unwrap();
        assert!(s.starts_with("GET /test HTTP/1.1\r\n"));
        assert!(s.contains("Host: example.com\r\n"));
        assert!(s.contains("Accept: text/html\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn serialize_post_with_body() {
        let body = b"hello world";
        let data = serialize_request("POST", "/submit", "example.com", &[], Some(body));
        let s = String::from_utf8(data).unwrap();
        assert!(s.contains("Content-Length: 11\r\n"));
        assert!(s.ends_with("hello world"));
    }

    #[test]
    fn parse_simple_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let mut parser = ResponseParser::new();
        match parser.feed(raw).unwrap() {
            FeedResult::Complete(resp) => {
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body, b"hello");
            }
            other => panic!("expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn parse_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut parser = ResponseParser::new();
        match parser.feed(raw).unwrap() {
            FeedResult::Complete(resp) => {
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body, b"hello world");
            }
            other => panic!("expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn parse_split_headers() {
        let mut parser = ResponseParser::new();
        // Feed headers in two parts
        match parser.feed(b"HTTP/1.1 200 OK\r\nContent").unwrap() {
            FeedResult::NeedMore => {}
            other => panic!("expected NeedMore, got {:?}", other),
        }
        match parser.feed(b"-Length: 3\r\n\r\nabc").unwrap() {
            FeedResult::Complete(resp) => {
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body, b"abc");
            }
            other => panic!("expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn parse_204_no_body() {
        let raw = b"HTTP/1.1 204 No Content\r\n\r\n";
        let mut parser = ResponseParser::new();
        match parser.feed(raw).unwrap() {
            FeedResult::Complete(resp) => {
                assert_eq!(resp.status, 204);
                assert!(resp.body.is_empty());
            }
            other => panic!("expected Complete, got {:?}", other),
        }
    }

    #[test]
    fn parse_chunked_split_across_feeds() {
        let mut parser = ResponseParser::new();
        // Headers + partial chunk
        match parser
            .feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel")
            .unwrap()
        {
            FeedResult::NeedMore => {}
            other => panic!("expected NeedMore, got {:?}", other),
        }
        // Rest of first chunk + second chunk + terminator
        match parser.feed(b"lo\r\n3\r\nabc\r\n0\r\n\r\n").unwrap() {
            FeedResult::Complete(resp) => {
                assert_eq!(resp.body, b"helloabc");
            }
            other => panic!("expected Complete, got {:?}", other),
        }
    }
}
