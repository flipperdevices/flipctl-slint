// Shared browser side for both views.
//
// Extracted because the two pages had their own copies of the frame loop, and a
// fix landing in one and not the other bit twice: the heartbeat skip, and the
// input handler. One file, both pages.

// Frames are an 8-byte little-endian header (x, y, w, h) then w*h bytes of 8-bit
// greyscale: exactly what the panel received. A zero-area region is a keepalive
// for a still screen and carries nothing to draw.
const FLIPPER_HEADER = 8;

/// Blit greyscale into a canvas.
///
/// Reuses one ImageData and writes a whole pixel per store through a Uint32
/// view. The obvious version allocates 147 KB per frame and writes each channel
/// separately, which is four stores per pixel and 36864 iterations of allocation
/// churn on every update.
class GreyBlitter {
  /// `tint` multiplies each channel, as [r, g, b] out of 255. The default is
  /// white, which leaves the panel's own grey alone.
  ///
  /// Tinting here rather than in CSS is what keeps the amber view cheap. The panel
  /// is greyscale, so the amber look is a per-channel multiply, and a `mix-blend-mode:
  /// multiply` layer made the browser re-blend and recomposite the whole stack
  /// (including the device photo above it) on every frame. Folding it into the
  /// pixel write costs nothing: the value becomes a table lookup, which is fewer
  /// operations than the shifts it replaces.
  constructor(ctx, w, h, tint) {
    this.ctx = ctx;
    this.setTint(tint || [255, 255, 255]);
    this.resize(w, h);
  }
  setTint(tint) {
    // Little-endian byte order in a word is A B G R.
    this.lut = new Uint32Array(256);
    for (let v = 0; v < 256; v++) {
      const r = (tint[0] * v / 255) | 0;
      const g = (tint[1] * v / 255) | 0;
      const b = (tint[2] * v / 255) | 0;
      this.lut[v] = 0xff000000 | (b << 16) | (g << 8) | r;
    }
  }
  resize(w, h) {
    this.img = this.ctx.createImageData(w, h);
    this.words = new Uint32Array(this.img.data.buffer);
  }
  draw(grey, x, y, w, h) {
    // putImageData blits the whole ImageData, so it has to be exactly the size of
    // the rect being drawn. The server sends whole frames today, so this reuses
    // the same buffer every time; a partial rect would otherwise paste a
    // full-width image at the rect's offset.
    if (this.img.width !== w || this.img.height !== h) this.resize(w, h);
    const words = this.words;
    const lut = this.lut;
    for (let i = 0; i < grey.length; i++) words[i] = lut[grey[i]];
    this.ctx.putImageData(this.img, x, y);
  }
}

/// Keep a connection to the frame stream, calling `onFrame(grey, x, y, w, h)`.
///
/// `onStatus(text, ok)` is handed a short description for a HUD and whether the
/// stream is live: connecting (ok undefined, nothing known yet), connected, or no
/// signal. Reconnects on its own,
/// because a device that restarts mid-session should not need a page reload.
///
/// A deadline is what makes that true. A rebooting device never closes the
/// connection, so the read it was parked on waited forever and only a reload got
/// out. The device sends a heartbeat every second on a still screen, so anything
/// silent past FLIPPER_DEADLINE_MS is dead and is dropped and retried.
const FLIPPER_DEADLINE_MS = 3000;

function flipperStream(onFrame, onStatus) {
  const say = (text, ok) => { if (onStatus) onStatus(text, ok); };
  let ctl = null;
  let heard = 0;
  setInterval(() => {
    if (ctl && performance.now() - heard > FLIPPER_DEADLINE_MS) ctl.abort();
  }, 250);
  (async () => {
    say('connecting');
    for (;;) {
      let live = false;
      ctl = new AbortController();
      heard = performance.now();
      try {
        const res = await fetch('stream', { signal: ctl.signal, cache: 'no-store' });
        if (!res.ok || !res.body) throw new Error('stream ' + res.status);
        const reader = res.body.getReader();
        let buf = new Uint8Array(0);
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          heard = performance.now();
          if (!live) {
            live = true;
            say('connected', true);
          }
          const merged = new Uint8Array(buf.length + value.length);
          merged.set(buf);
          merged.set(value, buf.length);
          buf = merged;
          for (;;) {
            if (buf.length < FLIPPER_HEADER) break;
            const head = new DataView(buf.buffer, buf.byteOffset, FLIPPER_HEADER);
            const x = head.getUint16(0, true), y = head.getUint16(2, true);
            const w = head.getUint16(4, true), h = head.getUint16(6, true);
            const need = FLIPPER_HEADER + w * h;
            if (buf.length < need) break;
            if (w !== 0 && h !== 0) onFrame(buf.subarray(FLIPPER_HEADER, need), x, y, w, h);
            buf = buf.subarray(need);
          }
        }
      } catch (e) {
        // Refused, timed out or cut off: all the same to a viewer.
      }
      ctl = null;
      say('no signal', false);
      await new Promise(r => setTimeout(r, 1000));
    }
  })();
}

/// Post one key event. The same endpoint the panel's own buttons feed.
function flipperSend(key, down) {
  fetch('input', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ key, down }),
  }).catch(() => {});
}

/// Wire a set of buttons carrying `data-k`, held while the pointer is down like
/// the real key, plus the keyboard equivalents.
const FLIPPER_KEYS = {
  ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left', ArrowRight: 'right',
  Enter: 'ok', Escape: 'back', Backspace: 'back', Tab: 'appsw',
  z: 'esc', x: 'view', c: 'power', v: 'edit', b: 'run', a: 'ptt',
};

function flipperWireInput(selector) {
  const held = key =>
    document.querySelector(`${selector}[data-k="${key}"]`);
  for (const el of document.querySelectorAll(selector)) {
    const key = el.dataset.k;
    const press = down => {
      el.classList.toggle('down', down);
      flipperSend(key, down);
    };
    el.addEventListener('pointerdown', e => { e.preventDefault(); press(true); });
    el.addEventListener('pointerup', () => press(false));
    el.addEventListener('pointercancel', () => press(false));
    el.addEventListener('pointerleave', () => { if (el.classList.contains('down')) press(false); });
    el.addEventListener('keydown', e => { if (e.key === ' ') { e.preventDefault(); press(true); } });
    el.addEventListener('keyup', e => { if (e.key === ' ') { e.preventDefault(); press(false); } });
  }
  // Keys currently down, so a key held while the page loses focus can be let go
  // of. Without it, alt-tabbing away mid-press leaves the device holding a button
  // nobody is pressing.
  const down = new Set();
  const send = (k, isDown) => {
    if (isDown) down.add(k); else down.delete(k);
    const el = held(k);
    if (el) el.classList.toggle('down', isDown);
    flipperSend(k, isDown);
  };
  // A modifier means the shortcut belongs to the browser or the window manager,
  // not to the device: alt-tab is the window switcher, and forwarding its Tab
  // opened the app switcher on the panel every time the user changed windows.
  const ours = e => !e.altKey && !e.ctrlKey && !e.metaKey
    && e.target.tagName !== 'BUTTON' && e.target.tagName !== 'SELECT';

  addEventListener('keydown', e => {
    if (!ours(e)) return;
    const k = FLIPPER_KEYS[e.key];
    if (k && !e.repeat) {
      e.preventDefault();
      send(k, true);
    }
  });
  addEventListener('keyup', e => {
    if (!ours(e)) return;
    const k = FLIPPER_KEYS[e.key];
    if (k) {
      e.preventDefault();
      send(k, false);
    }
  });
  // Leaving the page releases whatever was held. The keyup lands in the other
  // window, so it would never arrive here.
  addEventListener('blur', () => {
    for (const k of [...down]) send(k, false);
  });
}

/// Coalesce repeated work onto one animation frame.
///
/// The overlay was recomputing on every stream frame and every peer poll, up to
/// twenty full-screen passes a second for a display that can show sixty.
function flipperCoalesce(fn) {
  let queued = false;
  return () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; fn(); });
  };
}

/// The tab bar both pages open with, so each is one click from the other.
///
/// Built here rather than written into each page, so the two cannot drift apart.
/// `side` is the page's own status and controls, moved into the bar's right end.
/// The links are relative, as the old one was, so the pages still find each other
/// behind a proxy that serves them under a prefix.
function flipperTabs(side) {
  const style = document.createElement('style');
  style.textContent = `
    nav.tabs {
      position: fixed; top: 0; left: 0; right: 0; z-index: 10; height: 48px;
      display: flex; align-items: center; gap: 8px; padding: 0 12px;
      background: #1D2026; border-bottom: 1px solid #2B2F37;
      font: 600 15px system-ui, sans-serif; user-select: none; box-sizing: border-box;
    }
    nav.tabs > a {
      padding: 6px 12px; border-radius: 6px;
      color: #FF8200; text-decoration: none;
    }
    nav.tabs > a:hover { background: rgba(255, 130, 0, .12); }
    nav.tabs > a.here, nav.tabs > a.here:hover { background: #FF8200; color: #000; }
    nav.tabs > .tabs-side {
      margin-left: auto; display: flex; align-items: center; gap: 12px;
      min-width: 0; white-space: nowrap; overflow: hidden;
    }
    body { padding-top: 48px; box-sizing: border-box; }
  `;
  document.head.appendChild(style);

  const nav = document.createElement('nav');
  nav.className = 'tabs';
  const apps = /\/apps(\.html)?$/.test(location.pathname);
  for (const [href, label, here] of [['./', 'FlipCTL Remote Control', !apps], ['apps', 'App Manager', apps]]) {
    const a = document.createElement('a');
    a.href = href;
    a.textContent = label;
    if (here) a.className = 'here';
    nav.appendChild(a);
  }
  if (side) {
    side.classList.add('tabs-side');
    nav.appendChild(side);
  }
  document.body.prepend(nav);
}
