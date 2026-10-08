//! Cast V2 protocol: `CastMessage` protobuf framing over TLS (port 8009) and the
//! handful of JSON namespaces Fono8 needs (connection, heartbeat, receiver, media).
//!
//! The message schema is small enough to encode by hand, which keeps the build
//! free of `protoc`. Receivers use self-signed certificates, so the TLS client
//! deliberately skips certificate verification, like every Cast sender does.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use serde_json::{json, Value};

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";
pub const SENDER_ID: &str = "sender-fono8";
pub const RECEIVER_ID: &str = "receiver-0";
const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CastMessage {
    pub source: String,
    pub destination: String,
    pub namespace: String,
    pub payload: String,
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_string(out: &mut Vec<u8>, field: u8, value: &str) {
    put_varint(out, ((field as u64) << 3) | 2);
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

fn get_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *data.get(*pos)?;
        *pos += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

impl CastMessage {
    pub fn new(destination: &str, namespace: &str, payload: &Value) -> Self {
        CastMessage {
            source: SENDER_ID.into(),
            destination: destination.into(),
            namespace: namespace.into(),
            payload: payload.to_string(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(64 + self.payload.len());
        put_varint(&mut body, 1 << 3); // protocol_version = CASTV2_1_0
        put_varint(&mut body, 0);
        put_string(&mut body, 2, &self.source);
        put_string(&mut body, 3, &self.destination);
        put_string(&mut body, 4, &self.namespace);
        put_varint(&mut body, 5 << 3); // payload_type = STRING
        put_varint(&mut body, 0);
        put_string(&mut body, 6, &self.payload);
        let mut framed = Vec::with_capacity(body.len() + 4);
        framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
        framed.extend_from_slice(&body);
        framed
    }

    pub fn decode(data: &[u8]) -> Option<CastMessage> {
        let mut message = CastMessage::default();
        let mut pos = 0;
        while pos < data.len() {
            let key = get_varint(data, &mut pos)?;
            let field = key >> 3;
            match key & 7 {
                0 => {
                    get_varint(data, &mut pos)?;
                }
                2 => {
                    let len = get_varint(data, &mut pos)? as usize;
                    let bytes = data.get(pos..pos.checked_add(len)?)?;
                    pos += len;
                    let text = || String::from_utf8_lossy(bytes).into_owned();
                    match field {
                        2 => message.source = text(),
                        3 => message.destination = text(),
                        4 => message.namespace = text(),
                        6 => message.payload = text(),
                        _ => {}
                    }
                }
                1 => pos = pos.checked_add(8)?,
                5 => pos = pos.checked_add(4)?,
                _ => return None,
            }
        }
        Some(message)
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.payload).ok()
    }

    #[cfg(test)]
    pub fn kind(&self) -> String {
        self.json().and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string)).unwrap_or_default()
    }
}

/// Accepts any certificate: Cast receivers present self-signed, device-specific certs.
#[derive(Debug)]
struct TrustAnyCertificate;

impl ServerCertVerifier for TrustAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

fn tls_config() -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAnyCertificate))
        .with_no_client_auth();
    Arc::new(config)
}

/// A framed, polled TLS channel to one receiver.
pub struct Channel {
    stream: StreamOwned<ClientConnection, TcpStream>,
    buffer: Vec<u8>,
    request_id: u64,
}

impl Channel {
    pub fn connect(address: IpAddr, port: u16, timeout: Duration) -> std::io::Result<Channel> {
        let socket = TcpStream::connect_timeout(&SocketAddr::new(address, port), timeout)?;
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        socket.set_write_timeout(Some(timeout))?;
        let connection = ClientConnection::new(tls_config(), ServerName::IpAddress(address.into()))
            .map_err(|e| std::io::Error::new(ErrorKind::Other, e.to_string()))?;
        let mut stream = StreamOwned::new(connection, socket);
        // Finish the handshake now so connection errors surface immediately.
        let deadline = Instant::now() + timeout;
        while stream.conn.is_handshaking() {
            match stream.conn.complete_io(&mut stream.sock) {
                Ok(_) => {}
                Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if Instant::now() > deadline {
                        return Err(std::io::Error::new(ErrorKind::TimedOut, "TLS handshake timed out"));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Channel { stream, buffer: Vec::new(), request_id: 0 })
    }

    pub fn next_request_id(&mut self) -> u64 {
        self.request_id += 1;
        self.request_id
    }

    pub fn send(&mut self, destination: &str, namespace: &str, payload: Value) -> std::io::Result<()> {
        let message = CastMessage::new(destination, namespace, &payload);
        self.stream.write_all(&message.encode())?;
        self.stream.flush()
    }

    /// Non-blocking-ish poll: returns the next complete message, if one arrived.
    pub fn receive(&mut self) -> std::io::Result<Option<CastMessage>> {
        if let Some(message) = self.take_message() {
            return Ok(Some(message));
        }
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err(std::io::Error::new(ErrorKind::ConnectionAborted, "receiver closed the connection")),
            Ok(n) => {
                self.buffer.extend_from_slice(&chunk[..n]);
                Ok(self.take_message())
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn take_message(&mut self) -> Option<CastMessage> {
        if self.buffer.len() < 4 {
            return None;
        }
        let len = u32::from_be_bytes([self.buffer[0], self.buffer[1], self.buffer[2], self.buffer[3]]) as usize;
        if len > MAX_MESSAGE {
            // A corrupt frame: drop the buffer rather than wait forever.
            self.buffer.clear();
            return None;
        }
        if self.buffer.len() < 4 + len {
            return None;
        }
        let body: Vec<u8> = self.buffer.drain(..4 + len).skip(4).collect();
        CastMessage::decode(&body)
    }

    // ----- namespace helpers --------------------------------------------

    pub fn connect_to(&mut self, destination: &str) -> std::io::Result<()> {
        self.send(destination, NS_CONNECTION, json!({"type": "CONNECT", "origin": {}, "userAgent": "Fono8"}))
    }

    pub fn close_to(&mut self, destination: &str) -> std::io::Result<()> {
        self.send(destination, NS_CONNECTION, json!({"type": "CLOSE"}))
    }

    pub fn ping(&mut self) -> std::io::Result<()> {
        self.send(RECEIVER_ID, NS_HEARTBEAT, json!({"type": "PING"}))
    }

    pub fn pong(&mut self) -> std::io::Result<()> {
        self.send(RECEIVER_ID, NS_HEARTBEAT, json!({"type": "PONG"}))
    }

    pub fn receiver_status(&mut self) -> std::io::Result<u64> {
        let id = self.next_request_id();
        self.send(RECEIVER_ID, NS_RECEIVER, json!({"type": "GET_STATUS", "requestId": id}))?;
        Ok(id)
    }

    pub fn launch(&mut self, app_id: &str) -> std::io::Result<u64> {
        let id = self.next_request_id();
        self.send(RECEIVER_ID, NS_RECEIVER, json!({"type": "LAUNCH", "appId": app_id, "requestId": id}))?;
        Ok(id)
    }

    pub fn set_volume(&mut self, level: f32) -> std::io::Result<u64> {
        let id = self.next_request_id();
        self.send(
            RECEIVER_ID,
            NS_RECEIVER,
            json!({"type": "SET_VOLUME", "volume": {"level": level.clamp(0.0, 1.0)}, "requestId": id}),
        )?;
        Ok(id)
    }

    pub fn set_muted(&mut self, muted: bool) -> std::io::Result<u64> {
        let id = self.next_request_id();
        self.send(RECEIVER_ID, NS_RECEIVER, json!({"type": "SET_VOLUME", "volume": {"muted": muted}, "requestId": id}))?;
        Ok(id)
    }

    pub fn load(&mut self, transport: &str, url: &str, content_type: &str, title: &str) -> std::io::Result<u64> {
        let id = self.next_request_id();
        self.send(
            transport,
            NS_MEDIA,
            json!({
                "type": "LOAD",
                "requestId": id,
                "autoplay": true,
                "media": {
                    "contentId": url,
                    "contentType": content_type,
                    "streamType": "LIVE",
                    "metadata": {"metadataType": 0, "title": title}
                }
            }),
        )?;
        Ok(id)
    }

    pub fn media_status(&mut self, transport: &str) -> std::io::Result<u64> {
        let id = self.next_request_id();
        self.send(transport, NS_MEDIA, json!({"type": "GET_STATUS", "requestId": id}))?;
        Ok(id)
    }

    pub fn stop_media(&mut self, transport: &str, media_session: u64) -> std::io::Result<u64> {
        let id = self.next_request_id();
        self.send(transport, NS_MEDIA, json!({"type": "STOP", "mediaSessionId": media_session, "requestId": id}))?;
        Ok(id)
    }
}

/// The application entry Fono8 cares about in a `RECEIVER_STATUS`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReceiverApp {
    pub app_id: String,
    pub session_id: String,
    pub transport_id: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ReceiverStatus {
    pub apps: Vec<ReceiverApp>,
    pub volume_level: Option<f32>,
    pub muted: Option<bool>,
}

pub fn parse_receiver_status(payload: &Value) -> Option<ReceiverStatus> {
    let status = payload.get("status")?;
    let apps = status
        .get("applications")
        .and_then(Value::as_array)
        .map(|apps| {
            apps.iter()
                .filter_map(|app| {
                    Some(ReceiverApp {
                        app_id: app.get("appId")?.as_str()?.to_string(),
                        session_id: app.get("sessionId")?.as_str()?.to_string(),
                        transport_id: app.get("transportId")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let volume = status.get("volume");
    Some(ReceiverStatus {
        apps,
        volume_level: volume.and_then(|v| v.get("level")).and_then(Value::as_f64).map(|v| v as f32),
        muted: volume.and_then(|v| v.get("muted")).and_then(Value::as_bool),
    })
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct MediaStatus {
    pub media_session_id: Option<u64>,
    pub player_state: String,
    pub content_id: String,
    pub idle_reason: String,
}

/// The first entry of a `MEDIA_STATUS` message; `None` when the status list is empty.
pub fn parse_media_status(payload: &Value) -> Option<MediaStatus> {
    let entry = payload.get("status")?.as_array()?.first()?;
    Some(MediaStatus {
        media_session_id: entry.get("mediaSessionId").and_then(Value::as_u64),
        player_state: entry.get("playerState").and_then(Value::as_str).unwrap_or_default().to_string(),
        content_id: entry.get("media").and_then(|m| m.get("contentId")).and_then(Value::as_str).unwrap_or_default().to_string(),
        idle_reason: entry.get("idleReason").and_then(Value::as_str).unwrap_or_default().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrip() {
        let message = CastMessage::new("receiver-0", NS_HEARTBEAT, &json!({"type": "PING"}));
        let encoded = message.encode();
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        assert_eq!(len + 4, encoded.len());
        let decoded = CastMessage::decode(&encoded[4..]).unwrap();
        assert_eq!(decoded, message);
        assert_eq!(decoded.kind(), "PING");
    }

    #[test]
    fn decode_rejects_truncated_frames() {
        let encoded = CastMessage::new("a", "b", &json!({})).encode();
        assert!(CastMessage::decode(&encoded[4..encoded.len() - 2]).is_none());
    }

    #[test]
    fn parses_receiver_and_media_status() {
        let receiver = json!({
            "type": "RECEIVER_STATUS", "requestId": 2,
            "status": {
                "applications": [{"appId": "CC1AD845", "sessionId": "s1", "transportId": "t1", "displayName": "Default Media Receiver"}],
                "volume": {"level": 0.35, "muted": false}
            }
        });
        let status = parse_receiver_status(&receiver).unwrap();
        assert_eq!(status.apps[0].transport_id, "t1");
        assert_eq!(status.volume_level, Some(0.35));
        assert_eq!(status.muted, Some(false));
        let media = json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 7, "playerState": "PLAYING", "media": {"contentId": "http://x/live.mp3"}}]});
        let status = parse_media_status(&media).unwrap();
        assert_eq!(status.media_session_id, Some(7));
        assert_eq!(status.player_state, "PLAYING");
        assert_eq!(status.content_id, "http://x/live.mp3");
        assert!(parse_media_status(&json!({"type": "MEDIA_STATUS", "status": []})).is_none());
    }
}
