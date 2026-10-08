// Installed before the website creates/caches any MediaSource methods.
// Limit decoded playback to one track; never inspect or export media bytes.
(() => {
    if (location.origin !== 'https://music.youtube.com' || window.__fono8MediaBoundary) return;
    const boundary = {end: 0, buffers: 0, limited: 0, errors: 0};
    window.__fono8MediaBoundary = boundary;
    if (!window.MediaSource || !window.SourceBuffer) return;
    const tracked = new WeakMap();
    const add = MediaSource.prototype.addSourceBuffer;
    const append = SourceBuffer.prototype.appendBuffer;
    MediaSource.prototype.addSourceBuffer = function(type) {
        const buffer = add.call(this, type);
        if (typeof type === 'string' && type.startsWith('audio/')) {
            const state = {restore: null};
            tracked.set(buffer, state);
            boundary.buffers++;
            // Restore the site's window before its updateend handlers append
            // again. The next append will be independently bounded below.
            state.restoreWindow = () => {
                if (!state.restore) return;
                const [start, end] = state.restore;
                state.restore = null;
                try {
                    buffer.appendWindowStart = 0;
                    buffer.appendWindowEnd = end;
                    buffer.appendWindowStart = start;
                } catch { boundary.errors++; }
            };
            buffer.addEventListener('updateend', state.restoreWindow, true);
        }
        return buffer;
    };
    SourceBuffer.prototype.appendBuffer = function(bytes) {
        const state = tracked.get(this);
        let adjusted = false;
        if (state && boundary.end > 0 && !this.updating) {
            const start = this.appendWindowStart, end = this.appendWindowEnd;
            state.restore = [start, end];
            try {
                if (start >= boundary.end) this.appendWindowStart = 0;
                this.appendWindowEnd = Math.min(end, boundary.end);
                adjusted = true;
                boundary.limited++;
            } catch {
                boundary.errors++;
                state.restoreWindow();
            }
        }
        // Preserve native buffering events and errors, including empty windows.
        // Chromium discards frames beyond the current song's end itself.
        try {
            return append.call(this, bytes);
        } catch (error) {
            if (adjusted) state.restoreWindow();
            throw error;
        }
    };
})();
