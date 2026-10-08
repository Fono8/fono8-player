// Runs in an isolated world on music.youtube.com only.
// Credentials remain in the renderer. Never return cookies or headers to the host.
(async () => {
    const request = REQUEST;
    const store = globalThis.__fono8Requests ||= Object.create(null);
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 15000);
    store[request.id] = {pending: true, controller};
    try {
        if (location.origin !== 'https://music.youtube.com') throw new Error();
        const headers = {'Content-Type': 'application/json', 'X-Origin': location.origin};
        const cookies = Object.fromEntries(document.cookie.split(';').map(item => {
            const i = item.indexOf('=');
            return [item.slice(0, i).trim(), item.slice(i + 1)];
        }));
        const sid = cookies.SAPISID || cookies['__Secure-3PAPISID'];
        if (sid) {
            const timestamp = Math.floor(Date.now() / 1000);
            const input = new TextEncoder().encode(`${timestamp} ${sid} ${location.origin}`);
            const digest = await crypto.subtle.digest('SHA-1', input);
            const hash = Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2, '0')).join('');
            headers.Authorization = `SAPISIDHASH ${timestamp}_${hash}`;
            headers['X-Goog-AuthUser'] = request.account;
        }
        const response = await fetch('/youtubei/v1/' + request.endpoint + '?prettyPrint=false', {
            method: 'POST', credentials: 'same-origin', redirect: 'error',
            headers, body: JSON.stringify(request.body), signal: controller.signal
        });
        if (!response.ok) {
            store[request.id] = {error: response.status === 401 || response.status === 403
                ? 'yt_auth_error' : 'yt_network_error'};
            return;
        }
        const reader = response.body.getReader();
        const chunks = [];
        let size = 0;
        for (;;) {
            const {done, value} = await reader.read();
            if (done) break;
            size += value.length;
            if (size > 8 * 1024 * 1024) { await reader.cancel(); throw new Error(); }
            chunks.push(value);
        }
        const bytes = new Uint8Array(size);
        let offset = 0;
        for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
        store[request.id] = {data: JSON.parse(new TextDecoder().decode(bytes))};
    } catch (_) {
        store[request.id] = {error: 'yt_network_error'};
    } finally {
        clearTimeout(timer);
    }
})();
