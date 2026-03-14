use rustls::pki_types::ServerName;
use std::io::{Read, Write};
use std::sync::Arc;

/// Bridge between rustls (a state machine) and Wisp (JS byte channel).
/// Shuttles bytes between rustls and the JS side — no I/O of its own.
pub struct TlsBridge {
    conn: rustls::ClientConnection,
    incoming_buf: Vec<u8>,
}

impl TlsBridge {
    /// Create a new TLS connection for the given hostname.
    /// Uses Mozilla's CA roots for certificate validation.
    pub fn new(hostname: &str) -> Result<Self, String> {
        let root_store =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        let server_name = ServerName::try_from(hostname.to_string())
            .map_err(|e| format!("invalid hostname '{}': {}", hostname, e))?;

        let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
            .map_err(|e| format!("TLS connection setup failed: {}", e))?;

        conn.set_buffer_limit(Some(32768));

        Ok(Self {
            conn,
            incoming_buf: Vec::with_capacity(16384),
        })
    }

    /// Feed ciphertext bytes received from the Wisp stream into rustls.
    /// Loops to consume all complete TLS records in the buffer.
    pub fn feed_ciphertext(&mut self, data: &[u8]) -> Result<(), String> {
        self.incoming_buf.extend_from_slice(data);

        loop {
            if self.incoming_buf.is_empty() {
                break;
            }

            let mut cursor = std::io::Cursor::new(&self.incoming_buf);
            let bytes_read = self
                .conn
                .read_tls(&mut cursor)
                .map_err(|e| format!("read_tls failed: {}", e))?;

            if bytes_read == 0 {
                // Not enough data for a complete TLS record — wait for more
                break;
            }

            self.incoming_buf.drain(..bytes_read);
            self.conn
                .process_new_packets()
                .map_err(|e| format!("TLS error: {}", e))?;
        }

        Ok(())
    }

    /// Read decrypted plaintext bytes from rustls. May return empty vec.
    pub fn take_plaintext(&mut self) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut reader = self.conn.reader();
        let _ = reader.read_to_end(&mut buf);
        buf
    }

    /// Write plaintext bytes into rustls for encryption.
    pub fn feed_plaintext(&mut self, data: &[u8]) -> Result<(), String> {
        self.conn
            .writer()
            .write_all(data)
            .map_err(|e| format!("write_all failed: {}", e))?;
        Ok(())
    }

    /// Read encrypted ciphertext bytes from rustls to send via Wisp.
    pub fn take_ciphertext(&mut self) -> Vec<u8> {
        let mut buf = Vec::new();
        let _ = self.conn.write_tls(&mut buf);
        buf
    }

    /// Returns true if rustls has ciphertext to send.
    pub fn wants_write(&self) -> bool {
        self.conn.wants_write()
    }

    /// Returns true if the TLS handshake is still in progress.
    pub fn is_handshaking(&self) -> bool {
        self.conn.is_handshaking()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_client_hello() {
        let mut bridge = TlsBridge::new("example.com").expect("should create bridge");
        assert!(bridge.is_handshaking());
        let hello = bridge.take_ciphertext();
        assert!(!hello.is_empty(), "should produce ClientHello bytes");
        // TLS record starts with 0x16 (handshake)
        assert_eq!(hello[0], 0x16, "first byte should be TLS handshake record type");
    }

    #[test]
    fn rejects_invalid_hostname() {
        let result = TlsBridge::new("");
        assert!(result.is_err());
    }

    #[test]
    fn handles_garbage_input() {
        let mut bridge = TlsBridge::new("example.com").expect("should create bridge");
        let _ = bridge.take_ciphertext(); // drain ClientHello
        let result = bridge.feed_ciphertext(&[0xff, 0x00, 0x01, 0x02, 0x03]);
        // Should either error or buffer — not panic
        let _ = result;
    }

    #[test]
    fn client_hello_is_valid_tls_record() {
        let mut bridge = TlsBridge::new("example.com").unwrap();
        let hello = bridge.take_ciphertext();

        // TLS record: type(1) + version(2) + length(2) + data
        assert!(hello.len() >= 5, "too short for TLS record");
        assert_eq!(hello[0], 0x16, "should be handshake record type");

        // Version should be TLS 1.0 (0x0301) in the record layer
        // (even for TLS 1.2/1.3, the record layer version is 1.0)
        assert_eq!(hello[1], 0x03);

        let record_len = ((hello[3] as usize) << 8) | (hello[4] as usize);
        assert!(
            record_len > 0,
            "record should have non-zero content length"
        );
    }

    #[test]
    fn wants_write_after_creation() {
        let bridge = TlsBridge::new("example.com").unwrap();
        assert!(
            bridge.wants_write(),
            "should want to write ClientHello"
        );
    }

    #[test]
    fn no_plaintext_before_handshake() {
        let mut bridge = TlsBridge::new("example.com").unwrap();
        let pt = bridge.take_plaintext();
        assert!(pt.is_empty(), "no plaintext available before handshake");
    }

    #[test]
    fn take_ciphertext_drains() {
        let mut bridge = TlsBridge::new("example.com").unwrap();
        let first = bridge.take_ciphertext();
        assert!(!first.is_empty());
        let second = bridge.take_ciphertext();
        assert!(second.is_empty(), "should be empty after draining");
    }

    #[test]
    fn feed_empty_ciphertext_is_ok() {
        let mut bridge = TlsBridge::new("example.com").unwrap();
        let _ = bridge.take_ciphertext();
        // Feeding empty data should not error or panic
        bridge.feed_ciphertext(&[]).unwrap();
    }

    #[test]
    fn multiple_bridges_independent() {
        let mut b1 = TlsBridge::new("example.com").unwrap();
        let mut b2 = TlsBridge::new("other.com").unwrap();
        let h1 = b1.take_ciphertext();
        let h2 = b2.take_ciphertext();
        // Both should produce ClientHello, and they should differ (different SNI)
        assert!(!h1.is_empty());
        assert!(!h2.is_empty());
        // The ClientHello messages should be different due to different SNI
        assert_ne!(h1, h2);
    }

    #[test]
    fn feed_plaintext_before_handshake_complete() {
        let mut bridge = TlsBridge::new("example.com").unwrap();
        let _ = bridge.take_ciphertext();
        // Writing plaintext while still handshaking should not panic
        // (rustls may buffer or error gracefully)
        let result = bridge.feed_plaintext(b"GET / HTTP/1.1\r\n\r\n");
        // It might succeed (buffered) or fail — either is fine, just shouldn't panic
        let _ = result;
    }

    #[test]
    fn various_valid_hostnames() {
        // Various valid hostnames should work
        assert!(TlsBridge::new("example.com").is_ok());
        assert!(TlsBridge::new("sub.domain.example.com").is_ok());
        assert!(TlsBridge::new("api.anthropic.com").is_ok());
        assert!(TlsBridge::new("localhost").is_ok());
        assert!(TlsBridge::new("127.0.0.1").is_ok());
    }

    #[test]
    fn rejects_various_invalid_hostnames() {
        assert!(TlsBridge::new("").is_err());
    }

    #[test]
    fn handles_large_garbage_input() {
        let mut bridge = TlsBridge::new("example.com").unwrap();
        let _ = bridge.take_ciphertext();
        // Feed a large block of random-looking data
        let garbage = vec![0xAB; 10_000];
        let result = bridge.feed_ciphertext(&garbage);
        // Should error or buffer, not panic
        let _ = result;
    }

    #[test]
    fn partial_tls_record_buffered() {
        let mut bridge = TlsBridge::new("example.com").unwrap();
        let _ = bridge.take_ciphertext(); // drain ClientHello

        // Feed a partial TLS record header (just 3 bytes of what should be 5+ bytes)
        // Type 0x16 (handshake), version 0x0303 (TLS 1.2)
        let partial = [0x16, 0x03, 0x03];
        let result = bridge.feed_ciphertext(&partial);
        // Should not error — just buffer the partial data
        // (read_tls returns 0 when there's not enough for a record)
        assert!(result.is_ok(), "partial record should be buffered, not rejected");
    }
}
