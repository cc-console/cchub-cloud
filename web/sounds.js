// Shared alert-sound engine for cc-console.
//
// Every sound is *synthesized* with the Web Audio API — there are no audio asset
// files to bundle, so the daemon stays a single self-contained binary. Used by the
// console (web/index.html, the "your turn" alert) and the settings page
// (web/settings.html, the sound picker + preview).
//
// Selection + volume persist in localStorage:
//   cc-sound  → one of SOUNDS[].id   (default "chirp")
//   cc-volume → "0".."1" master gain (default 0.8)
window.CCSound = (function () {
  // The picker order. zh/en labels are rendered by the settings page.
  const SOUNDS = [
    { id: 'chirp',   zh: '滴滴（默认）', en: 'Chirp (default)' },
    { id: 'ding',    zh: '叮',           en: 'Ding' },
    { id: 'chime',   zh: '风铃',         en: 'Chime' },
    { id: 'marimba', zh: '马林巴',       en: 'Marimba' },
    { id: 'pop',     zh: '气泡',         en: 'Pop' },
    { id: 'glass',   zh: '水晶',         en: 'Glass' },
  ];
  const DEFAULT_ID = 'chirp';
  const DEFAULT_VOL = 0.8;

  let ctx = null;
  // AudioContext can only start after a user gesture; callers prime it on the first
  // interaction (console) or it's created on the first preview click (settings).
  function ensureCtx() {
    try {
      ctx = ctx || new (window.AudioContext || window.webkitAudioContext)();
      if (ctx.state === 'suspended') ctx.resume();
    } catch (_) {}
    return ctx;
  }

  // One enveloped oscillator partial. `peak` is pre-master gain; `glideTo` bends the
  // pitch over the note for "pop". Times are absolute (AudioContext currentTime).
  function partial(c, master, o) {
    const osc = c.createOscillator(), g = c.createGain();
    const t0 = o.t0, dur = o.dur, attack = o.attack || 0.012, peak = o.peak || 0.3;
    osc.type = o.type || 'sine';
    osc.frequency.setValueAtTime(o.freq, t0);
    if (o.glideTo) osc.frequency.exponentialRampToValueAtTime(o.glideTo, t0 + dur);
    g.gain.setValueAtTime(0.0001, t0);
    g.gain.exponentialRampToValueAtTime(peak, t0 + attack);
    g.gain.exponentialRampToValueAtTime(0.0001, t0 + dur);
    osc.connect(g); g.connect(master);
    osc.start(t0); osc.stop(t0 + dur + 0.03);
  }

  // Each recipe schedules its partials relative to `t` (the start time).
  const RECIPES = {
    // The original two rising tones — kept as the default so nothing changes for
    // existing users.
    chirp(c, m, t) {
      partial(c, m, { freq: 880,  t0: t,        dur: 0.18, peak: 0.35 });
      partial(c, m, { freq: 1175, t0: t + 0.20, dur: 0.22, peak: 0.35 });
    },
    // A clean bell: fundamental + bright partials with a long exponential decay.
    ding(c, m, t) {
      partial(c, m, { freq: 1568, t0: t, dur: 0.70, peak: 0.30 });
      partial(c, m, { freq: 3136, t0: t, dur: 0.45, peak: 0.09 });
      partial(c, m, { freq: 4704, t0: t, dur: 0.30, peak: 0.04 });
    },
    // Three soft ascending notes (C5–E5–G5), wind-chime style.
    chime(c, m, t) {
      [523.25, 659.25, 783.99].forEach((f, i) =>
        partial(c, m, { type: 'triangle', freq: f, t0: t + i * 0.12, dur: 0.50, peak: 0.26 }));
    },
    // Woody, percussive double tap.
    marimba(c, m, t) {
      partial(c, m, { type: 'triangle', freq: 587.33, t0: t,        dur: 0.18, peak: 0.34 });
      partial(c, m, { type: 'triangle', freq: 880.00, t0: t + 0.13, dur: 0.20, peak: 0.34 });
    },
    // A short, low-key bubble with a quick upward pitch bend.
    pop(c, m, t) {
      partial(c, m, { freq: 420, t0: t, dur: 0.12, peak: 0.40, attack: 0.006, glideTo: 720 });
    },
    // Shimmering high crystal bell.
    glass(c, m, t) {
      partial(c, m, { freq: 2093, t0: t,        dur: 1.00, peak: 0.22 });
      partial(c, m, { freq: 3136, t0: t + 0.02, dur: 0.80, peak: 0.07 });
    },
  };

  // Play `id` at `volume` (0..1). Each call routes its partials through a fresh
  // master gain so overlapping plays don't fight over one node.
  function play(id, volume) {
    const c = ensureCtx();
    if (!c) return;
    if (c.state === 'suspended') c.resume();
    const v = (volume == null) ? DEFAULT_VOL : Math.max(0, Math.min(1, volume));
    if (v <= 0) return;                       // muted
    const recipe = RECIPES[id] || RECIPES[DEFAULT_ID];
    const master = c.createGain();
    master.gain.value = v;
    master.connect(c.destination);
    recipe(c, master, c.currentTime + 0.02);
  }

  // Read the persisted selection + volume (with fallbacks).
  function fromStorage() {
    let id = DEFAULT_ID, vol = DEFAULT_VOL;
    try {
      id = localStorage.getItem('cc-sound') || DEFAULT_ID;
      const v = parseFloat(localStorage.getItem('cc-volume'));
      if (!isNaN(v)) vol = Math.max(0, Math.min(1, v));
    } catch (_) {}
    if (!RECIPES[id]) id = DEFAULT_ID;
    return { id, vol };
  }

  // Convenience for the console: play whatever the user has saved.
  function playSaved() { const s = fromStorage(); play(s.id, s.vol); }

  return { SOUNDS, DEFAULT_ID, DEFAULT_VOL, ensureCtx, play, playSaved, fromStorage };
})();
