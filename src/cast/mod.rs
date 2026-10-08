//! Google Cast output: discovery, connection lifecycle and the audio relay,
//! isolated from the UI thread. Audio is captured inside the player, so it works
//! the same on Linux, macOS and Windows.

pub mod protocol;
pub mod stream;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde_json::Value;

use crate::audio::CaptureSink;
use crate::i18n::Message;
use protocol::{
    parse_media_status, parse_receiver_status, Channel, DEFAULT_MEDIA_RECEIVER, NS_CONNECTION, NS_HEARTBEAT, NS_MEDIA,
    NS_RECEIVER, RECEIVER_ID,
};
use stream::AudioStream;

const SERVICE_TYPE: &str = "_googlecast._tcp.local.";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const PLAY_TIMEOUT: Duration = Duration::from_secs(25);
const LOST_TIMEOUT: Duration = Duration::from_secs(8);
const SCAN_DURATION: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastState {
    Local,
    Connecting,
    Connected,
    Disconnecting,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastDevice {
    pub id: String,
    pub name: String,
    pub model: String,
    pub address: IpAddr,
    pub port: u16,
}

/// What the UI shows; replaced wholesale by the worker, polled by the model.
#[derive(Clone, Debug, PartialEq)]
pub struct CastStatus {
    pub state: CastState,
    pub name: String,
    pub message: Message,
    pub devices: Vec<CastDevice>,
    pub scanning: bool,
    pub volume: u32,
    pub muted: bool,
    pub volume_known: bool,
    pub revision: u64,
}

impl Default for CastStatus {
    fn default() -> Self {
        CastStatus {
            state: CastState::Local,
            name: String::new(),
            message: Message::new("cast_local"),
            devices: Vec::new(),
            scanning: false,
            volume: 0,
            muted: false,
            volume_known: false,
            revision: 0,
        }
    }
}

enum Command {
    Scan,
    Output { generation: u64, device: Option<String> },
    Volume { generation: u64, value: u32 },
    Mute { generation: u64, muted: bool },
    Close,
}

pub struct CastOutput {
    commands: Option<Sender<Command>>,
    status: Arc<Mutex<CastStatus>>,
    capture: CaptureSink,
    generation: u64,
    thread: Option<std::thread::JoinHandle<()>>,
    last_revision: u64,
}

impl CastOutput {
    pub fn new(capture: CaptureSink) -> CastOutput {
        CastOutput {
            commands: None,
            status: Arc::new(Mutex::new(CastStatus::default())),
            capture,
            generation: 0,
            thread: None,
            last_revision: 0,
        }
    }

    fn ensure_worker(&mut self) {
        if self.commands.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let status = self.status.clone();
        let capture = self.capture.clone();
        self.thread = std::thread::Builder::new()
            .name("fono8-cast".into())
            .spawn(move || Worker::new(receiver, status, capture).run())
            .ok();
        self.commands = Some(sender);
    }

    pub fn status(&self) -> CastStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// `true` when the status changed since the last call.
    pub fn poll(&mut self) -> bool {
        let revision = self.status.lock().map(|s| s.revision).unwrap_or(0);
        if revision != self.last_revision {
            self.last_revision = revision;
            true
        } else {
            false
        }
    }

    pub fn discover(&mut self) {
        self.ensure_worker();
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Scan);
        }
    }

    /// Route audio to a device, or back to this computer with `None`.
    pub fn select(&mut self, device: Option<String>) {
        self.ensure_worker();
        self.generation += 1;
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Output { generation: self.generation, device });
        }
    }

    pub fn set_volume(&self, value: u32) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Volume { generation: self.generation, value: value.min(100) });
        }
    }

    pub fn toggle_mute(&self) {
        let muted = self.status().muted;
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Mute { generation: self.generation, muted: !muted });
        }
    }

    pub fn close(&mut self) {
        self.generation += 1;
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(Command::Close);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CastOutput {
    fn drop(&mut self) {
        self.close();
    }
}

struct Session {
    generation: u64,
    device: CastDevice,
    channel: Channel,
    transport: String,
    stream: AudioStream,
    media_session: Option<u64>,
    last_ok: Instant,
    last_ping: Instant,
    last_poll: Instant,
    reported_volume: Option<(u32, bool)>,
}

struct Worker {
    commands: Receiver<Command>,
    status: Arc<Mutex<CastStatus>>,
    capture: CaptureSink,
    daemon: Option<ServiceDaemon>,
    events: Option<mdns_sd::Receiver<ServiceEvent>>,
    devices: HashMap<String, CastDevice>,
    scan_deadline: Option<Instant>,
    session: Option<Session>,
    generation: u64,
    /// A command received while a connection attempt was cancelled; handled next.
    pending: Option<Command>,
}

#[derive(Debug)]
enum Failure {
    Cancelled,
    Error(&'static str),
}

impl From<&'static str> for Failure {
    fn from(key: &'static str) -> Self {
        Failure::Error(key)
    }
}

impl From<std::io::Error> for Failure {
    fn from(_: std::io::Error) -> Self {
        Failure::Error("cast_connect_error")
    }
}

impl Worker {
    fn new(commands: Receiver<Command>, status: Arc<Mutex<CastStatus>>, capture: CaptureSink) -> Worker {
        Worker {
            commands,
            status,
            capture,
            daemon: None,
            events: None,
            devices: HashMap::new(),
            scan_deadline: None,
            session: None,
            generation: 0,
            pending: None,
        }
    }

    fn update(&self, f: impl FnOnce(&mut CastStatus)) {
        if let Ok(mut status) = self.status.lock() {
            f(&mut status);
            status.revision += 1;
        }
    }

    fn run(mut self) {
        loop {
            let command = match self.pending.take() {
                Some(command) => Some(command),
                None => match self.commands.recv_timeout(Duration::from_millis(150)) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
            };
            match command {
                Some(Command::Close) => break,
                Some(Command::Scan) => {
                    if let Err(error) = self.discover() {
                        crate::app::debug(|| format!("cast discovery failed: {error}"));
                        self.update(|s| {
                            s.scanning = false;
                            s.message = Message::new("cast_discovery_error");
                        });
                    }
                }
                Some(Command::Output { generation, device }) => {
                    if self.session.is_some() {
                        self.update(|s| {
                            s.state = CastState::Disconnecting;
                            s.volume_known = false;
                            s.message = Message::new("cast_disconnecting");
                        });
                    }
                    self.disconnect();
                    self.generation = generation;
                    match device {
                        None => self.update(|s| {
                            s.state = CastState::Local;
                            s.name.clear();
                            s.volume_known = false;
                            s.message = Message::new("cast_local");
                        }),
                        Some(id) => match self.connect(generation, &id) {
                            Ok(()) => {}
                            Err(Failure::Cancelled) => self.disconnect(),
                            Err(Failure::Error(key)) => {
                                crate::app::debug(|| format!("cast connect failed: {key}"));
                                self.disconnect();
                                self.update(|s| {
                                    s.state = CastState::Local;
                                    s.name.clear();
                                    s.volume_known = false;
                                    s.message = Message::new(key);
                                });
                            }
                        },
                    }
                }
                Some(Command::Volume { generation, value }) => self.set_device_volume(generation, Some(value), None),
                Some(Command::Mute { generation, muted }) => self.set_device_volume(generation, None, Some(muted)),
                None => {}
            }
            self.poll_discovery();
            if let Err(key) = self.monitor() {
                crate::app::debug(|| format!("cast session ended: {key}"));
                self.disconnect();
                self.update(|s| {
                    s.state = CastState::Local;
                    s.name.clear();
                    s.volume_known = false;
                    s.message = Message::new(key);
                });
            }
            if let Some(deadline) = self.scan_deadline {
                if Instant::now() >= deadline {
                    self.scan_deadline = None;
                    self.update(|s| s.scanning = false);
                }
            }
        }
        self.disconnect();
        if let Some(daemon) = self.daemon.take() {
            let _ = daemon.shutdown();
        }
    }

    // ----- discovery ------------------------------------------------------

    fn discover(&mut self) -> Result<(), mdns_sd::Error> {
        if self.daemon.is_none() {
            let daemon = ServiceDaemon::new()?;
            self.events = Some(daemon.browse(SERVICE_TYPE)?);
            self.daemon = Some(daemon);
        }
        self.scan_deadline = Some(Instant::now() + SCAN_DURATION);
        let devices = self.sorted_devices();
        self.update(|s| {
            s.scanning = true;
            s.devices = devices;
        });
        Ok(())
    }

    fn sorted_devices(&self) -> Vec<CastDevice> {
        let mut devices: Vec<CastDevice> = self.devices.values().cloned().collect();
        devices.sort_by(|a, b| (a.name.to_lowercase(), &a.id).cmp(&(b.name.to_lowercase(), &b.id)));
        devices
    }

    fn poll_discovery(&mut self) {
        let Some(events) = &self.events else { return };
        let mut changed = false;
        while let Ok(event) = events.try_recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let property = |key: &str| info.txt_properties.get_property_val_str(key).unwrap_or("").to_string();
                    let id = {
                        let id = property("id");
                        if id.is_empty() {
                            info.fullname.clone()
                        } else {
                            id
                        }
                    };
                    let address = info
                        .addresses
                        .iter()
                        .map(|a| a.to_ip_addr())
                        .find(|a| a.is_ipv4())
                        .or_else(|| info.addresses.iter().map(|a| a.to_ip_addr()).next());
                    if let Some(address) = address {
                        let name = {
                            let name = property("fn");
                            if name.is_empty() {
                                "Google Cast".to_string()
                            } else {
                                name
                            }
                        };
                        self.devices.insert(id.clone(), CastDevice { id, name, model: property("md"), address, port: info.port });
                        changed = true;
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let before = self.devices.len();
                    self.devices.retain(|id, _| *id != fullname);
                    changed |= before != self.devices.len();
                }
                _ => {}
            }
        }
        if changed {
            let devices = self.sorted_devices();
            self.update(|s| s.devices = devices);
        }
    }

    // ----- connection -----------------------------------------------------

    /// A newer Output or Close command supersedes the attempt in progress.
    fn cancelled(&mut self, generation: u64) -> Result<(), Failure> {
        if self.generation != generation {
            return Err(Failure::Cancelled);
        }
        match self.commands.try_recv() {
            Ok(command @ (Command::Output { .. } | Command::Close)) => {
                self.pending = Some(command);
                Err(Failure::Cancelled)
            }
            Ok(Command::Scan) => {
                self.pending = Some(Command::Scan);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn wait_for<T>(
        &mut self,
        generation: u64,
        timeout: Duration,
        error: &'static str,
        mut check: impl FnMut(&mut Session) -> Result<Option<T>, Failure>,
    ) -> Result<T, Failure> {
        let deadline = Instant::now() + timeout;
        loop {
            self.cancelled(generation)?;
            let session = self.session.as_mut().ok_or(Failure::Error(error))?;
            if let Some(value) = check(session)? {
                return Ok(value);
            }
            if Instant::now() > deadline {
                return Err(Failure::Error(error));
            }
            self.pump_messages()?;
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn connect(&mut self, generation: u64, id: &str) -> Result<(), Failure> {
        let device = self.devices.get(id).cloned().ok_or(Failure::Error("cast_device_missing"))?;
        let name = device.name.clone();
        self.update(|s| {
            s.state = CastState::Connecting;
            s.name = name.clone();
            s.volume_known = false;
            s.message = Message::new("cast_connecting").with("device", name.clone());
        });
        let local = stream::local_address(device.address, device.port).ok_or(Failure::Error("cast_network_error"))?;
        let mut channel = Channel::connect(device.address, device.port, CONNECT_TIMEOUT)?;
        channel.connect_to(RECEIVER_ID)?;
        channel.receiver_status()?;
        channel.launch(DEFAULT_MEDIA_RECEIVER)?;
        let (sender, receiver) = mpsc::sync_channel::<Vec<f32>>(256);
        let stream = AudioStream::open(local, receiver).map_err(Failure::Error)?;
        self.session = Some(Session {
            generation,
            device,
            channel,
            transport: String::new(),
            stream,
            media_session: None,
            last_ok: Instant::now(),
            last_ping: Instant::now(),
            last_poll: Instant::now(),
            reported_volume: None,
        });
        let transport = self.wait_for(generation, LAUNCH_TIMEOUT, "cast_connect_error", |session| {
            Ok(if session.transport.is_empty() { None } else { Some(session.transport.clone()) })
        })?;
        {
            let session = self.session.as_mut().unwrap();
            session.channel.connect_to(&transport)?;
            let url = session.stream.url.clone();
            session.channel.load(&transport, &url, "audio/mpeg", "Fono8")?;
        }
        // Start feeding the encoder now; local output stays audible until the receiver plays.
        if let Ok(mut slot) = self.capture.lock() {
            *slot = Some(sender);
        }
        self.wait_for(generation, PLAY_TIMEOUT, "cast_stream_error", |session| {
            session.stream.check().map_err(Failure::Error)?;
            let playing = session.media_session.is_some() && session.last_ok.elapsed() < Duration::from_secs(1);
            Ok((playing && session.stream.requested()).then_some(()))
        })?;
        crate::app::debug(|| "cast: receiver is playing the relay".into());
        self.update(|s| {
            s.state = CastState::Connected;
            s.message = Message::new("cast_connected").with("device", s.name.clone());
        });
        self.session.as_mut().unwrap().last_ok = Instant::now();
        Ok(())
    }

    /// Read and act on everything the receiver sent. Errors mean the session is gone.
    fn pump_messages(&mut self) -> Result<(), Failure> {
        let Some(session) = self.session.as_mut() else { return Ok(()) };
        let mut volume_update = None;
        loop {
            let message = match session.channel.receive() {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(_) => return Err(Failure::Error("cast_connection_lost")),
            };
            let payload = message.json().unwrap_or(Value::Null);
            let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
            match (message.namespace.as_str(), kind) {
                (NS_HEARTBEAT, "PING") => session.channel.pong()?,
                (NS_CONNECTION, "CLOSE") => {
                    if message.source == session.transport {
                        return Err(Failure::Error("cast_connection_lost"));
                    }
                }
                (NS_RECEIVER, "RECEIVER_STATUS") => {
                    if let Some(status) = parse_receiver_status(&payload) {
                        if let Some(app) = status.apps.iter().find(|app| app.app_id == DEFAULT_MEDIA_RECEIVER) {
                            if session.transport.is_empty() {
                                session.transport = app.transport_id.clone();
                            } else if session.transport != app.transport_id {
                                // Another sender replaced our session.
                                return Err(Failure::Error("cast_connection_lost"));
                            }
                        } else if !session.transport.is_empty() {
                            return Err(Failure::Error("cast_connection_lost"));
                        }
                        if let (Some(level), Some(muted)) = (status.volume_level, status.muted) {
                            volume_update = Some(((level.clamp(0.0, 1.0) * 100.0).round() as u32, muted));
                        }
                    }
                }
                (NS_RECEIVER, "LAUNCH_ERROR") => return Err(Failure::Error("cast_connect_error")),
                (NS_MEDIA, "MEDIA_STATUS") => match parse_media_status(&payload) {
                    Some(status) if status.content_id == session.stream.url => {
                        session.media_session = status.media_session_id;
                        if status.player_state == "PLAYING" || status.player_state == "BUFFERING" {
                            session.last_ok = Instant::now();
                        } else if status.player_state == "IDLE"
                            && !status.idle_reason.is_empty()
                            && status.idle_reason != "INTERRUPTED"
                        {
                            return Err(Failure::Error("cast_connection_lost"));
                        }
                    }
                    Some(_) => {
                        if session.media_session.is_some() {
                            return Err(Failure::Error("cast_connection_lost"));
                        }
                    }
                    None => {
                        if session.media_session.is_some() {
                            return Err(Failure::Error("cast_connection_lost"));
                        }
                    }
                },
                (NS_MEDIA, "LOAD_FAILED") | (NS_MEDIA, "LOAD_CANCELLED") | (NS_MEDIA, "INVALID_REQUEST") => {
                    return Err(Failure::Error("cast_stream_error"));
                }
                _ => {}
            }
        }
        if let Some((volume, muted)) = volume_update {
            if session.reported_volume != Some((volume, muted)) {
                session.reported_volume = Some((volume, muted));
                // Also recorded while connecting, so the panel has a value as soon as it is connected.
                self.update(|s| {
                    s.volume = volume;
                    s.muted = muted;
                    s.volume_known = true;
                    if s.message.key == "cast_volume_error" {
                        s.message = Message::new("cast_connected").with("device", s.name.clone());
                    }
                });
            }
        }
        Ok(())
    }

    fn monitor(&mut self) -> Result<(), &'static str> {
        let connected = self.status.lock().map(|s| s.state == CastState::Connected).unwrap_or(false);
        if !connected || self.session.is_none() {
            return Ok(());
        }
        if let Err(Failure::Error(key)) = self.pump_messages() {
            return Err(key);
        }
        let session = self.session.as_mut().unwrap();
        session.stream.check()?;
        if session.last_ping.elapsed() >= Duration::from_secs(5) {
            session.last_ping = Instant::now();
            session.channel.ping().map_err(|_| "cast_connection_lost")?;
        }
        if session.last_poll.elapsed() >= Duration::from_secs(3) {
            session.last_poll = Instant::now();
            let transport = session.transport.clone();
            session.channel.media_status(&transport).map_err(|_| "cast_connection_lost")?;
            session.channel.receiver_status().map_err(|_| "cast_connection_lost")?;
        }
        // A receiver paused by Google Home should not build an ever-growing delay.
        if session.last_ok.elapsed() > LOST_TIMEOUT {
            return Err("cast_connection_lost");
        }
        Ok(())
    }

    fn set_device_volume(&mut self, generation: u64, volume: Option<u32>, muted: Option<bool>) {
        // A command queued for a previous speaker must never affect its successor.
        let connected = self.status.lock().map(|s| s.state == CastState::Connected && s.volume_known).unwrap_or(false);
        let Some(session) = self.session.as_mut() else { return };
        if session.generation != generation || !connected {
            return;
        }
        let result = match (volume, muted) {
            (Some(value), _) => session.channel.set_volume(value as f32 / 100.0),
            (_, Some(muted)) => session.channel.set_muted(muted),
            _ => return,
        };
        match result {
            Ok(_) => {
                // Optimistic update; the receiver confirms through RECEIVER_STATUS.
                self.update(|s| {
                    if let Some(value) = volume {
                        s.volume = value;
                    }
                    if let Some(muted) = muted {
                        s.muted = muted;
                    }
                });
            }
            Err(_) => self.update(|s| s.message = Message::new("cast_volume_error")),
        }
    }

    fn disconnect(&mut self) {
        if let Ok(mut slot) = self.capture.lock() {
            *slot = None;
        }
        if let Some(mut session) = self.session.take() {
            // Stop only our content, never a session started by another sender.
            if let Some(media_session) = session.media_session {
                let transport = session.transport.clone();
                let _ = session.channel.stop_media(&transport, media_session);
            }
            if !session.transport.is_empty() {
                let transport = session.transport.clone();
                let _ = session.channel.close_to(&transport);
            }
            let _ = session.channel.close_to(RECEIVER_ID);
            session.stream.close();
            crate::app::debug(|| format!("cast: disconnected from {}", session.device.name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a Cast device on the LAN; run with `cargo test cast -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn discovers_and_streams_to_the_first_device() {
        let capture: CaptureSink = Arc::new(Mutex::new(None));
        let mut cast = CastOutput::new(capture.clone());
        cast.discover();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut devices = Vec::new();
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            devices = cast.status().devices;
            if !devices.is_empty() {
                break;
            }
        }
        println!("devices: {devices:?}");
        let Some(device) = devices.first().cloned() else {
            println!("no Cast device found; skipping");
            return;
        };
        cast.select(Some(device.id.clone()));
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let status = cast.status();
            if status.state == CastState::Connected {
                println!("connected: {status:?}");
                break;
            }
            if status.state == CastState::Local {
                panic!("connection failed: {:?}", status.message);
            }
            assert!(Instant::now() < deadline, "timed out in state {:?}", status.state);
        }
        assert!(capture.lock().unwrap().is_some(), "capture tap attached while casting");
        std::thread::sleep(Duration::from_secs(6));
        let status = cast.status();
        assert_eq!(status.state, CastState::Connected, "{:?}", status.message);
        println!("device volume: {} muted: {} known: {}", status.volume, status.muted, status.volume_known);
        cast.select(None);
        std::thread::sleep(Duration::from_secs(1));
        assert!(capture.lock().unwrap().is_none());
        cast.close();
    }
}
