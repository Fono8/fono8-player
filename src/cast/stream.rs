//! A bounded, RAM-only live MP3 relay fed by the audio engine's capture tap.
//!
//! The encoder runs on its own clock: every 50 ms it takes the PCM that arrived
//! from the player and pads with silence when playback is paused, so the
//! receiver keeps a steady stream. The HTTP server exposes exactly one random path, on the interface
//! that reaches the receiver, and never serves files or directories.

use std::collections::VecDeque;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Mode, Quality};

use crate::audio::{CAPTURE_CHANNELS, CAPTURE_RATE};

const FRAME_MS: u64 = 50;
const MAX_BACKLOG_SAMPLES: usize = (CAPTURE_RATE as usize) * (CAPTURE_CHANNELS as usize); // one second
const MAX_LISTENERS: usize = 4;
const LISTENER_CHUNKS: usize = 64;

/// Use the interface that actually reaches the selected IPv4 receiver.
pub fn local_address(host: IpAddr, port: u16) -> Option<IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect(SocketAddr::new(host, port)).ok()?;
    let address = probe.local_addr().ok()?.ip();
    if address.is_loopback() || address.is_unspecified() {
        return None;
    }
    Some(address)
}

/// Fan-out of encoded chunks to connected listeners; slow listeners are dropped.
#[derive(Default)]
struct Broadcast {
    listeners: Vec<SyncSender<Vec<u8>>>,
}

impl Broadcast {
    fn write(&mut self, chunk: &[u8]) {
        self.listeners.retain(|listener| match listener.try_send(chunk.to_vec()) {
            Ok(()) => true,
            // A slow receiver must reconnect, not accumulate latency.
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        });
    }

    fn subscribe(&mut self) -> Option<Receiver<Vec<u8>>> {
        if self.listeners.len() >= MAX_LISTENERS {
            return None;
        }
        let (sender, receiver) = mpsc::sync_channel(LISTENER_CHUNKS);
        self.listeners.push(sender);
        Some(receiver)
    }
}

struct ListenerBody {
    receiver: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    closed: Arc<AtomicBool>,
}

impl Read for ListenerBody {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pending.is_empty() {
            if self.closed.load(Ordering::Relaxed) {
                return Ok(0);
            }
            match self.receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(chunk) => self.pending = chunk,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = out.len().min(self.pending.len());
        out[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

pub struct AudioStream {
    pub url: String,
    closed: Arc<AtomicBool>,
    requested: Arc<AtomicBool>,
    failed: Arc<Mutex<Option<String>>>,
    server: Arc<tiny_http::Server>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl AudioStream {
    /// Start the encoder and HTTP relay; `pcm` is the capture tap's receiving end.
    pub fn open(address: IpAddr, pcm: Receiver<Vec<f32>>) -> Result<AudioStream, &'static str> {
        let server = tiny_http::Server::http(SocketAddr::new(address, 0)).map_err(|_| "cast_stream_error")?;
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            #[allow(unreachable_patterns)]
            _ => return Err("cast_stream_error"),
        };
        let token: String = {
            let mut bytes = [0u8; 24];
            rand::fill(&mut bytes);
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        };
        let path = format!("/{token}/live.mp3");
        let url = match address {
            IpAddr::V4(_) => format!("http://{address}:{port}{path}"),
            IpAddr::V6(_) => format!("http://[{address}]:{port}{path}"),
        };
        let encoder = Builder::new()
            .and_then(|b| b.with_num_channels(CAPTURE_CHANNELS as u8).ok())
            .and_then(|b| b.with_sample_rate(CAPTURE_RATE).ok())
            .and_then(|b| b.with_brate(Bitrate::Kbps192).ok())
            .and_then(|b| b.with_mode(Mode::JointStereo).ok())
            .and_then(|b| b.with_quality(Quality::Good).ok())
            .and_then(|b| b.build().ok())
            .ok_or("cast_encoder_error")?;
        let server = Arc::new(server);
        let broadcast = Arc::new(Mutex::new(Broadcast::default()));
        let closed = Arc::new(AtomicBool::new(false));
        let requested = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(Mutex::new(None));
        let mut threads = Vec::new();

        // Encoder: paced by wall-clock time, pads with silence while paused.
        let encoder_broadcast = broadcast.clone();
        let encoder_closed = closed.clone();
        let encoder_failed = failed.clone();
        threads.push(
            std::thread::Builder::new()
                .name("fono8-cast-encoder".into())
                .spawn(move || encode_loop(encoder, pcm, encoder_broadcast, encoder_closed, encoder_failed))
                .map_err(|_| "cast_encoder_error")?,
        );

        // HTTP: one path, chunked MP3, no directory or file access.
        let http_server = server.clone();
        let http_broadcast = broadcast.clone();
        let http_closed = closed.clone();
        let http_requested = requested.clone();
        threads.push(
            std::thread::Builder::new()
                .name("fono8-cast-http".into())
                .spawn(move || {
                    while !http_closed.load(Ordering::Relaxed) {
                        let request = match http_server.recv_timeout(Duration::from_millis(200)) {
                            Ok(Some(request)) => request,
                            Ok(None) => continue,
                            Err(_) => break,
                        };
                        let wanted = request.url() == path;
                        let listener = if wanted { http_broadcast.lock().ok().and_then(|mut b| b.subscribe()) } else { None };
                        match listener {
                            Some(receiver)
                                if request.method() == &tiny_http::Method::Get
                                    || request.method() == &tiny_http::Method::Head =>
                            {
                                http_requested.store(true, Ordering::Relaxed);
                                let headers = [
                                    "Content-Type: audio/mpeg",
                                    "Cache-Control: no-store",
                                    "Connection: close",
                                    "Accept-Ranges: none",
                                ]
                                .iter()
                                .filter_map(|h| {
                                    tiny_http::Header::from_bytes(
                                        &h.as_bytes()[..h.find(':').unwrap()],
                                        h[h.find(':').unwrap() + 2..].as_bytes(),
                                    )
                                    .ok()
                                })
                                .collect();
                                let body = ListenerBody { receiver, pending: Vec::new(), closed: http_closed.clone() };
                                let response = tiny_http::Response::new(tiny_http::StatusCode(200), headers, body, None, None);
                                // Each listener streams on its own thread so the accept loop stays responsive.
                                std::thread::spawn(move || {
                                    let _ = request.respond(response);
                                });
                            }
                            _ => {
                                let _ = request.respond(tiny_http::Response::empty(if wanted { 503 } else { 404 }));
                            }
                        }
                    }
                })
                .map_err(|_| "cast_stream_error")?,
        );
        Ok(AudioStream { url, closed, requested, failed, server, threads })
    }

    /// `true` once the receiver has fetched the stream at least once.
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    /// An encoder failure, reported once.
    pub fn check(&self) -> Result<(), &'static str> {
        match self.failed.lock().ok().and_then(|f| f.clone()) {
            Some(_) => Err("cast_encoder_error"),
            None => Ok(()),
        }
    }

    pub fn close(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        self.server.unblock();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        self.close();
    }
}

fn encode_loop(
    mut encoder: mp3lame_encoder::Encoder,
    pcm: Receiver<Vec<f32>>,
    broadcast: Arc<Mutex<Broadcast>>,
    closed: Arc<AtomicBool>,
    failed: Arc<Mutex<Option<String>>>,
) {
    let frame_samples = (CAPTURE_RATE as usize * FRAME_MS as usize / 1000) * CAPTURE_CHANNELS as usize;
    let mut backlog: VecDeque<f32> = VecDeque::with_capacity(MAX_BACKLOG_SAMPLES * 2);
    let mut frame = vec![0f32; frame_samples];
    let mut output = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(frame_samples / 2));
    let mut next_frame = Instant::now();
    while !closed.load(Ordering::Relaxed) {
        // Drain whatever the player produced since the last frame.
        loop {
            match pcm.try_recv() {
                Ok(chunk) => backlog.extend(chunk),
                Err(_) => break,
            }
        }
        if backlog.len() > MAX_BACKLOG_SAMPLES {
            // The player clock runs ahead of ours: drop the oldest audio instead of growing latency.
            let excess = backlog.len() - MAX_BACKLOG_SAMPLES;
            backlog.drain(..excess);
        }
        for sample in frame.iter_mut() {
            *sample = backlog.pop_front().unwrap_or(0.0);
        }
        output.clear();
        match encoder.encode_to_vec(InterleavedPcm(&frame), &mut output) {
            Ok(_) => {
                if !output.is_empty() {
                    if let Ok(mut b) = broadcast.lock() {
                        b.write(&output);
                    }
                }
            }
            Err(error) => {
                if let Ok(mut f) = failed.lock() {
                    *f = Some(error.to_string());
                }
                return;
            }
        }
        next_frame += Duration::from_millis(FRAME_MS);
        let now = Instant::now();
        if next_frame > now {
            match pcm.recv_timeout(next_frame - now) {
                Ok(chunk) => backlog.extend(chunk),
                Err(RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(next_frame.saturating_duration_since(Instant::now()));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            let remaining = next_frame.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                std::thread::sleep(remaining);
            }
        } else if now - next_frame > Duration::from_secs(2) {
            next_frame = now; // We fell far behind (suspend?); resynchronize.
        }
    }
    output.clear();
    let _ = encoder.flush_to_vec::<FlushNoGap>(&mut output);
    if let Ok(mut b) = broadcast.lock() {
        b.write(&output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Ipv4Addr, TcpStream};

    #[test]
    fn relays_mp3_to_a_listener_and_rejects_other_paths() {
        let (sender, receiver) = mpsc::sync_channel::<Vec<f32>>(64);
        let mut stream = AudioStream::open(IpAddr::V4(Ipv4Addr::LOCALHOST), receiver).expect("stream");
        let url = url::parse(&stream.url);
        // Push a short tone so the encoder has something besides silence.
        let tone: Vec<f32> = (0..44_100)
            .flat_map(|i| {
                let v = (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.5;
                [v, v]
            })
            .collect();
        sender.send(tone).unwrap();

        let mut socket = TcpStream::connect((url.host.as_str(), url.port)).unwrap();
        write!(socket, "GET {} HTTP/1.1\r\nHost: {}\r\n\r\n", url.path, url.host).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("HTTP/1.1 200"), "{line}");
        let mut headers = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            headers.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        assert!(headers.to_lowercase().contains("content-type: audio/mpeg"), "{headers}");
        let mut body = vec![0u8; 4096];
        let mut got = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while got < 2048 && Instant::now() < deadline {
            let n = reader.read(&mut body[got..]).unwrap();
            if n == 0 {
                break;
            }
            got += n;
        }
        assert!(got >= 2048, "only {got} bytes");
        // Chunked body: skip the first chunk-size line, then expect an MP3 sync word.
        let payload = &body[..got];
        let start = payload.iter().position(|b| *b == b'\n').map(|p| p + 1).unwrap_or(0);
        let sync = payload[start..].windows(2).any(|w| w[0] == 0xff && (w[1] & 0xe0) == 0xe0);
        assert!(sync, "no MP3 frame sync in relayed data");
        assert!(stream.requested());

        let mut other = TcpStream::connect((url.host.as_str(), url.port)).unwrap();
        write!(other, "GET /somewhere-else HTTP/1.1\r\nHost: {}\r\n\r\n", url.host).unwrap();
        let mut status = String::new();
        BufReader::new(other).read_line(&mut status).unwrap();
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
        stream.close();
    }

    mod url {
        pub struct Parts {
            pub host: String,
            pub port: u16,
            pub path: String,
        }

        pub fn parse(url: &str) -> Parts {
            let rest = url.strip_prefix("http://").unwrap();
            let (authority, path) = rest.split_once('/').unwrap();
            let (host, port) = authority.rsplit_once(':').unwrap();
            Parts { host: host.to_string(), port: port.parse().unwrap(), path: format!("/{path}") }
        }
    }
}
