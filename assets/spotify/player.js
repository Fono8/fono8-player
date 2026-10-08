/* Spotify owns audio decoding. The page exposes only playback commands and state. */
(() => {
  'use strict';
  let player = null;
  let events = [];
  let tokenWaiters = [];
  let requested = false;
  let ready = false;
  let allowed = false;
  let polling = false;
  let volume = 0.65;
  const emit = (kind, data = {}) => { events.push({kind, ...data}); events = events.slice(-32); };
  const failure = kind => emit('error', {code: kind});

  function stateChanged(state) {
    if (!state) { emit('state', {available: false}); return; }
    if (!allowed && !state.paused) {
      player.pause().catch(() => failure('playback_error'));
    }
    const track = state.track_window?.current_track;
    document.getElementById('title').textContent = track?.name || 'Spotify';
    document.getElementById('artist').textContent = (track?.artists || []).map(a => a.name).join(', ');
    const image = track?.album?.images?.[0]?.url;
    const cover = document.getElementById('cover');
    if (image && /^https:\/\/[a-z0-9.-]+\.scdn\.co\//i.test(image)) cover.src = image;
    else cover.removeAttribute('src');
    const source = document.getElementById('source');
    const id = track?.id;
    source.hidden = !/^[a-zA-Z0-9]{22}$/.test(id || '');
    if (!source.hidden) source.href = 'https://open.spotify.com/track/' + id;
    emit('state', {
      available: true, paused: state.paused, position: state.position,
      duration: state.duration, uri: track?.uri || '', title: track?.name || '',
      original_uri: track?.linked_from?.uri || '',
      volume,
      disallows: state.disallows || {}
    });
  }

  function connect() {
    requested = true;
    if (!window.Spotify || player) return;
    const current = new Spotify.Player({
      name: 'Fono8', volume: 0.65,
      getOAuthToken: callback => {
        if (player !== current) return;
        tokenWaiters.push(callback);
        if (tokenWaiters.length === 1) emit('token');
      }
    });
    player = current;
    const listen = (name, callback) => current.addListener(name, data => {
      if (player === current) callback(data);
    });
    listen('ready', ({device_id}) => { ready = true; emit('ready', {device_id}); });
    listen('not_ready', () => { ready = false; emit('not_ready'); });
    listen('player_state_changed', stateChanged);
    for (const name of ['initialization_error', 'authentication_error', 'account_error',
                        'playback_error', 'autoplay_failed']) listen(name, () => failure(name));
    current.connect().then(ok => {
      if (player === current && !ok) failure('initialization_error');
    }).catch(() => { if (player === current) failure('initialization_error'); });
  }

  function command(name, value) {
    if (!player || !ready) return;
    let result;
    if (name === 'pause') { allowed = false; result = player.pause(); }
    else if (name === 'arm') { allowed = true; result = player.activateElement(); }
    else if (name === 'resume') {
      allowed = true;
      const current = player;
      result = Promise.resolve(current.activateElement()).then(() => {
        if (player === current && allowed) return current.resume();
      });
    }
    else if (name === 'seek') result = player.seek(Math.max(0, Number(value) || 0));
    else if (name === 'volume') result = player.setVolume(Math.max(0, Math.min(1, Number(value) || 0)));
    if (result) result.catch(() => failure('playback_error'));
  }

  window.fono8Spotify = {
    connect, command,
    drain: () => { const value = events; events = []; return value; },
    provideToken: token => {
      const waiters = tokenWaiters; tokenWaiters = [];
      if (token) waiters.forEach(callback => callback(token));
    },
    disconnect: () => {
      allowed = ready = requested = false;
      tokenWaiters = [];
      const current = player; player = null;
      if (current) current.disconnect();
      events = [];
      document.getElementById('title').textContent = 'Spotify';
      document.getElementById('artist').textContent = '';
      document.getElementById('cover').removeAttribute('src');
      document.getElementById('source').hidden = true;
    }
  };
  window.onSpotifyWebPlaybackSDKReady = () => { emit('sdk_loaded'); if (requested) connect(); };
  const sdk = document.createElement('script');
  sdk.src = 'https://sdk.scdn.co/spotify-player.js';
  sdk.onerror = () => failure('sdk_load_error');
  document.head.appendChild(sdk);
  setInterval(() => {
    if (!player || !ready || polling) return;
    const current = player;
    polling = true;
    current.getVolume().then(value => {
      if (player === current && Number.isFinite(value)) volume = value;
    }).catch(() => {});
    current.getCurrentState().then(state => {
      if (player === current) stateChanged(state);
    }).catch(() => {}).finally(() => { polling = false; });
  }, 1000);
})();
