//! Scripts injected into the YouTube Music pages.

/// Runs the bounded same-origin API request inside the page (`REQUEST` is replaced).
pub const REQUEST: &str = include_str!("../assets/request.js");

/// Limits decoded playback to one track; installed before site scripts run.
pub const MEDIA_GUARD: &str = include_str!("../assets/media_guard.js");

pub const AUTH: &str = r#"(() => {
    if (location.origin !== 'https://music.youtube.com') return 'unknown';
    const loggedIn = window.ytcfg?.get('LOGGED_IN');
    return loggedIn === true ? 'signed_in' : loggedIn === false ? 'signed_out' : 'unknown';
})()"#;

/// Public client configuration only; never account tokens or cookies.
pub const CONFIG: &str = r#"JSON.stringify({
    context: window.ytcfg?.get('INNERTUBE_CONTEXT'),
    account: String(window.ytcfg?.get('SESSION_INDEX') ?? '0'),
    loggedIn: window.ytcfg?.get('LOGGED_IN') === true
})"#;

pub const POLL: &str = r#"(() => {
    const store = globalThis.__fono8Requests || {}, completed = {};
    for (const [id, result] of Object.entries(store)) {
        if (!result.pending) {completed[id] = result; delete store[id];}
    }
    return JSON.stringify(completed);
})()"#;

pub const ABORT_ALL: &str = "Object.values(globalThis.__fono8Requests || {}).forEach(r => r.controller?.abort()); globalThis.__fono8Requests = Object.create(null);";

/// Player state probe with the end guard (`PARAMETERS` is replaced).
pub const STATE: &str = r#"(() => {
    if (location.origin !== 'https://music.youtube.com') return '{}';
    const p = document.getElementById('movie_player');
    if (!p?.getVideoData || !p?.getPlayerState) return '{}';
    const {target, started, epoch} = PARAMETERS;
    const id = p.getVideoData().video_id;
    if (id !== target) {
        if (started) p.pauseVideo();
        return JSON.stringify({id});
    }
    const video = p.querySelector?.('video') || document.querySelector('video');
    let details = {}, audioDuration = null;
    try {
        const response = p.getPlayerResponse?.() || {};
        details = response.videoDetails || {};
        const audio = response.streamingData?.adaptiveFormats?.find(
            format => format.mimeType?.startsWith('audio/'));
        if (audio?.approxDurationMs) audioDuration = Number(audio.approxDurationMs) / 1000;
    } catch { /* Optional API. */ }
    const boundary = window.__fono8MediaBoundary;
    if (boundary && !(boundary.end > 0) && details.videoId === target
            && Number.isFinite(audioDuration) && audioDuration > 0 && audioDuration < 2000000)
        boundary.end = audioDuration;
    let guard = window.__fono8Playback;
    if (!guard || guard.target !== target || guard.epoch !== epoch || guard.video !== video) {
        if (guard?.video) {
            guard.video.removeEventListener('ended', guard.onEnded, true);
            guard.video.removeEventListener('timeupdate', guard.onBoundary, true);
            guard.video.removeEventListener('waiting', guard.onBoundary, true);
            window.clearInterval?.(guard.timer);
        }
        guard = {target, epoch, video, ended: false};
        guard.finish = () => {
            if (window.__fono8Playback !== guard) return;
            guard.ended = true;
            window.clearInterval?.(guard.timer);
            video.pause();
            p.pauseVideo();
        };
        guard.onEnded = (event) => {
            if (window.__fono8Playback !== guard) return;
            event.stopImmediatePropagation();
            guard.finish();
        };
        guard.onBoundary = (event) => {
            if (window.__fono8Playback !== guard || guard.ended || !video || video.seeking
                    || video.paused || !(boundary?.end > 0)) return;
            const end = boundary.end;
            const bufferedEnd = video.buffered?.length
                ? video.buffered.end(video.buffered.length - 1) : 0;
            const exhausted = event?.type === 'waiting' && bufferedEnd >= end - 0.05
                && video.currentTime >= bufferedEnd - 0.01;
            if (video.currentTime >= end || exhausted) guard.finish();
        };
        window.__fono8Playback = guard;
        if (video) {
            video.addEventListener('ended', guard.onEnded, true);
            video.addEventListener('timeupdate', guard.onBoundary, true);
            video.addEventListener('waiting', guard.onBoundary, true);
            guard.timer = window.setInterval?.(guard.onBoundary, 50);
        }
    }
    return JSON.stringify({id, state: p.getPlayerState(),
        position: p.getCurrentTime(), duration: p.getDuration(),
        seeking: video?.seeking === true,
        ended: guard.ended || video?.ended === true});
})()"#;

/// Loudness of five frequency bands (dB) of what the player is playing, for Fono8's logo.
/// The video element is routed through a Web Audio analyser once (it keeps playing
/// to the speakers through the same graph); only MediaSource streams (`blob:`), so a
/// cross-origin source can never be silenced. Returns `null` while nothing can be measured.
pub const LEVELS: &str = r#"(() => {
    if (location.origin !== 'https://music.youtube.com') return null;
    const p = document.getElementById('movie_player');
    const video = p?.querySelector?.('video') || document.querySelector('video');
    if (!video || video.paused || !String(video.currentSrc || video.src).startsWith('blob:')) return null;
    let meter = window.__fono8Meter || {};
    window.__fono8Meter = meter;
    if (meter.video !== video) {
        if (meter.failed === video) return null;
        try {
            meter.context ||= new (window.AudioContext || window.webkitAudioContext)();
            // Routing into a suspended context would silence the song.
            if (meter.context.state !== 'running') { meter.context.resume?.(); return null; }
            const source = meter.context.createMediaElementSource(video);
            const analyser = meter.context.createAnalyser();
            analyser.fftSize = 2048;
            analyser.smoothingTimeConstant = 0;
            source.connect(meter.context.destination);
            source.connect(analyser);
            Object.assign(meter, {video, analyser, data: new Float32Array(analyser.frequencyBinCount)});
        } catch {
            meter.failed = video;
            return null;
        }
    }
    if (meter.context.state !== 'running') meter.context.resume?.();
    meter.analyser.getFloatFrequencyData(meter.data);
    const bin = meter.context.sampleRate / meter.analyser.fftSize;
    return [90, 280, 900, 2800, 8000].map(centre => {
        const low = Math.max(1, Math.floor(centre / 1.6 / bin));
        const high = Math.min(meter.data.length - 1, Math.max(low, Math.ceil(centre * 1.6 / bin)));
        let power = 0;
        for (let i = low; i <= high; i++) power += Math.pow(10, meter.data[i] / 10);
        return 10 * Math.log10(power + 1e-12);
    });
})()"#;

/// Seek with the end guard re-armed (`PARAMETERS` is replaced).
pub const SEEK: &str = r#"(() => {
    if (location.origin !== 'https://music.youtube.com') return;
    const {target, position, epoch} = PARAMETERS;
    const p = document.getElementById('movie_player');
    if (!p?.getVideoData || p.getVideoData().video_id !== target) return;
    const video = p.querySelector?.('video') || document.querySelector('video');
    try {
        const guard = window.__fono8Playback;
        if (guard?.target === target && guard.video === video) {
            guard.epoch = epoch;
            guard.ended = false;
        }
        p.seekTo(position / 1000, true);
    } catch {}
})()"#;

/// The toolbar above the web views: notice, Home button, tabs and a navigation warning.
pub fn toolbar_html() -> String {
    r##"<!doctype html><html><head><meta charset="utf-8"><style>
html, body { margin: 0; height: 100%; background: #07101e; color: #e8f0ff; font: 12px 'Inter', 'Ubuntu', 'Segoe UI', sans-serif; overflow: hidden; user-select: none; }
.bar { display: flex; align-items: center; gap: 10px; padding: 6px 10px; height: 32px; }
.notice { flex: 1; min-width: 0; white-space: pre-line; color: #9baecb; font-size: 11px; line-height: 1.25; overflow: hidden; }
button { border: 1px solid #263d5e; border-radius: 7px; padding: 5px 12px; background: #102038; color: #e8f0ff; font: inherit; font-weight: 500; cursor: pointer; }
button:hover { background: #172d4b; border-color: #9b6dff; }
.tabs { display: flex; gap: 2px; padding: 0 10px; height: 20px; }
.tab { padding: 2px 14px; background: #0e1a2c; color: #9baecb; border: 1px solid #24334b; border-bottom: none; border-radius: 6px 6px 0 0; cursor: pointer; font-size: 11px; }
.tab.active { background: #242142; color: #e8f0ff; }
.tab:hover { color: #3de8f7; }
.warning { display: none; padding: 0 10px; color: #ffb4b4; font-size: 11px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
</style></head><body>
<div class="bar"><div class="notice" id="notice"></div><button id="home">Home</button></div>
<div class="tabs"><div class="tab active" id="tab0">Account</div><div class="tab" id="tab1">Player</div></div>
<div class="warning" id="warning"></div>
<script>
const post = (m) => window.ipc && window.ipc.postMessage(m);
document.getElementById('home').addEventListener('click', () => post('home'));
document.getElementById('tab0').addEventListener('click', () => post('tab:0'));
document.getElementById('tab1').addEventListener('click', () => post('tab:1'));
window.__fono8SetState = (s) => {
    const notice = document.getElementById('notice');
    notice.textContent = s.notice || '';
    notice.title = s.tooltip || '';
    document.getElementById('home').textContent = s.home || 'Home';
    document.getElementById('tab0').textContent = s.tabs?.[0] || 'Account';
    document.getElementById('tab1').textContent = s.tabs?.[1] || 'Player';
    document.getElementById('tab0').classList.toggle('active', s.current === 0);
    document.getElementById('tab1').classList.toggle('active', s.current === 1);
    const warning = document.getElementById('warning');
    warning.textContent = s.warning || '';
    warning.style.display = s.warning ? 'block' : 'none';
};
</script></body></html>"##
        .to_string()
}
