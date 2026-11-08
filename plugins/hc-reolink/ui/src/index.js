// hc-reolink's own screens: a camera's card and its panel, as plain custom
// elements (see the template plugin for the pattern).
//
//   hc-reolink-card   (device.card)   host, deviceId, form
//   hc-reolink-panel  (device.panel)  host, deviceId
//   hc-reolink-cameras (dashboard.card) host, form, config: several cameras
//
// Live video is go2rtc's, played from the app's own address (`/video`, which
// only passes it on for someone signed in): MSE over a WebSocket, the sub
// stream on cards and either on the panel. The newest snapshot is a file the
// plugin keeps (`host.fileUrl`). A camera's spotlight and siren are devices of
// their own (`<camera>_spotlight`, `<camera>_siren`), commanded as such.

const DETECTS = [
	['person_detected', 'Person'],
	['vehicle_detected', 'Vehicle'],
	['animal_detected', 'Animal'],
	['package_detected', 'Package'],
	['motion', 'Motion']
];

/** What a camera sees now, most telling first. */
function seeing(a) {
	return DETECTS.filter(([k]) => a[k] === true).map(([, label]) => label);
}

/** "3:42 PM" today, else "Nov 7, 3:42 PM". */
function when(iso) {
	if (!iso) return '';
	const d = new Date(iso);
	const today = new Date().toDateString() === d.toDateString();
	return d.toLocaleString(undefined, today ? { hour: 'numeric', minute: '2-digit' } : { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
}

// ---- live video: go2rtc's MSE over a WebSocket ----

const MS = window.ManagedMediaSource ?? window.MediaSource;
const CODECS = ['avc1.640029', 'avc1.64002A', 'avc1.640033', 'hvc1.1.6.L153.B0', 'mp4a.40.2', 'mp4a.40.5', 'opus'];

/** Plays one go2rtc stream into a <video>; `stop()` ends it. */
class Live {
	constructor(video, stream, onstate) {
		this.video = video;
		this.stream = stream;
		this.onstate = onstate;
		this.queue = [];
		this.start();
	}

	start() {
		if (!MS) return this.onstate('unsupported');
		this.onstate('connecting');
		const ms = new MS();
		this.ms = ms;
		this.video.disableRemotePlayback = true;
		this.video.src = URL.createObjectURL(ms);
		this.video.srcObject = null;
		ms.addEventListener('sourceopen', () => {
			URL.revokeObjectURL(this.video.src);
			const proto = location.protocol === 'https:' ? 'wss' : 'ws';
			const ws = new WebSocket(`${proto}://${location.host}/video/api/ws?src=${encodeURIComponent(this.stream)}`);
			this.ws = ws;
			ws.binaryType = 'arraybuffer';
			ws.onopen = () => {
				const codecs = CODECS.filter((c) => MS.isTypeSupported(`video/mp4; codecs="${c}"`));
				ws.send(JSON.stringify({ type: 'mse', value: codecs.join(',') }));
			};
			ws.onmessage = (e) => {
				if (typeof e.data === 'string') {
					const msg = JSON.parse(e.data);
					if (msg.type === 'mse') {
						this.sb = ms.addSourceBuffer(msg.value);
						this.sb.mode = 'segments';
						this.sb.addEventListener('updateend', () => this.pump());
					} else if (msg.type === 'error') {
						this.onstate('error');
					}
					return;
				}
				this.queue.push(e.data);
				this.pump();
			};
			ws.onclose = () => {
				if (!this.stopped) {
					this.onstate('lost');
					this.retry = setTimeout(() => this.restart(), 3000);
				}
			};
		}, { once: true });
		this.video.play().catch(() => {});
	}

	pump() {
		const sb = this.sb;
		if (!sb || sb.updating || this.queue.length === 0) return;
		try {
			sb.appendBuffer(this.queue.shift());
		} catch {
			this.queue = [];
			return;
		}
		const v = this.video;
		if (v.buffered.length) {
			const end = v.buffered.end(v.buffered.length - 1);
			// Keep to the live edge, and keep the buffer small.
			if (end - v.currentTime > 2) v.currentTime = end - 0.3;
			if (!sb.updating && v.currentTime - v.buffered.start(0) > 20) {
				try {
					sb.remove(v.buffered.start(0), v.currentTime - 5);
				} catch {}
			}
			if (v.readyState >= 2) this.onstate('live');
		}
	}

	restart() {
		this.teardown();
		this.queue = [];
		this.start();
	}

	teardown() {
		clearTimeout(this.retry);
		if (this.ws) {
			this.ws.onclose = null;
			this.ws.close();
		}
		this.ws = undefined;
		this.sb = undefined;
		this.video.removeAttribute('src');
		this.video.load();
	}

	stop() {
		this.stopped = true;
		this.teardown();
	}
}

// ---- shared ----

class Screen extends HTMLElement {
	#host;
	#stop;
	#deviceId;
	devices = [];

	constructor() {
		super();
		this.attachShadow({ mode: 'open' });
	}

	set host(h) {
		if (this.#host === h) return;
		this.#host = h;
		this.shadowRoot.adoptedStyleSheets = [h.theme.sheet];
		this.#stop?.();
		this.#stop = h.devices.subscribe((list) => {
			this.devices = list;
			this.draw();
		});
	}
	get host() {
		return this.#host;
	}
	set deviceId(v) {
		this.#deviceId = v;
		this.draw();
	}
	get deviceId() {
		return this.#deviceId;
	}

	get device() {
		return this.devices.find((d) => d.device_id === this.#deviceId);
	}
	part(suffix) {
		return this.devices.find((d) => d.device_id === `${this.#deviceId}_${suffix}`);
	}

	disconnectedCallback() {
		this.#stop?.();
		this.#stop = undefined;
		this.live?.stop();
		this.live = undefined;
	}

	esc(s) {
		return String(s ?? '').replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
	}

	snapUrl(a) {
		return a.snapshot && this.host.fileUrl ? this.host.fileUrl(a.snapshot) : '';
	}

	/** Start (or keep) live video of `stream` in the <video> drawn. */
	play(stream) {
		const video = this.shadowRoot.querySelector('video');
		if (!video || !stream) return;
		if (this.live && this.live.stream === stream && this.live.video === video) return;
		this.live?.stop();
		this.live = new Live(video, stream, (s) => {
			this.liveState = s;
			const tag = this.shadowRoot.querySelector('.live');
			if (tag) tag.textContent = s === 'live' ? 'Live' : s === 'connecting' ? 'Connecting…' : s === 'unsupported' ? 'Stills only here' : 'Reconnecting…';
			if (tag) tag.dataset.state = s;
		});
	}

	draw() {}
}

const STYLE = `
	:host { display: flex; flex-direction: column; height: 100%; }
	.view { position: relative; border-radius: 12px; overflow: hidden; background: #0a0c10; aspect-ratio: 16 / 9; }
	.view video, .view img { position: absolute; inset: 0; width: 100%; height: 100%; object-fit: cover; }
	.view img.under { z-index: 0; } .view video { z-index: 1; }
	.tag { position: absolute; z-index: 2; padding: 2px 9px; border-radius: 999px; background: rgba(0,0,0,.6); color: #fff; font-size: 12px; }
	.live { left: 8px; top: 8px; display: flex; gap: 6px; align-items: center; }
	.live::before { content: ''; width: 7px; height: 7px; border-radius: 50%; background: #9aa4b2; }
	.live[data-state=live]::before { background: #e5484d; }
	.seen { left: 8px; bottom: 8px; display: flex; gap: 5px; background: none; padding: 0; }
	.seen span { padding: 2px 9px; border-radius: 999px; background: var(--accent, #f2b544); color: var(--accent-ink, #1a1406); font-weight: 700; font-size: 12px; }
	.seen span.motion { background: rgba(255,255,255,.85); color: #111; }
	.off { position: absolute; inset: 0; display: grid; place-items: center; color: #9aa4b2; font-size: 13px; }
	.row { display: flex; align-items: center; gap: 8px; }
	.grow { flex: 1; min-width: 0; }
	.name { font-weight: 700; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
`;

// ---- the card ----

/** A camera's card: live at large, its newest still otherwise. */
class CameraCard extends Screen {
	#form = 'standard';
	set form(v) {
		this.#form = v;
		this.draw();
	}

	draw() {
		const h = this.host;
		const d = this.device;
		if (!h || !d) return;
		const a = d.attributes;
		const online = d.available !== false;
		const sees = seeing(a);
		const light = this.part('spotlight');
		const big = this.#form === 'large';
		if (this.#form === 'compact') {
			this.live?.stop();
			this.live = undefined;
			this.shadowRoot.innerHTML = `<style>${STYLE}</style>
				<button class="hc-tile compact" style="all: unset; cursor: pointer; display: flex; gap: 10px; align-items: center; width: 100%">
					<span class="grow"><span class="hc-tile-name">${this.esc(h.devices.label(d))}</span>
					<span class="hc-tile-state">${online ? (sees.length ? this.esc(sees.join(' · ')) : 'Quiet') : 'Offline'}</span></span>
				</button>`;
			this.shadowRoot.querySelector('button').onclick = () => h.ui.openDevice(d.device_id);
			return;
		}
		const snap = this.snapUrl(a);
		const showLive = big && online && a.stream;
		const keep = showLive && this.live && this.shadowRoot.querySelector('video');
		if (!keep) {
			this.live?.stop();
			this.live = undefined;
			this.shadowRoot.innerHTML = `<style>${STYLE}
					.card { display: flex; flex-direction: column; gap: 8px; flex: 1; box-sizing: border-box; }
					.card.big .view { flex: 1; min-height: 160px; aspect-ratio: auto; }
					/* Narrow, the buttons go under the name rather than over it. */
					.foot { flex-wrap: wrap; }
					.foot .grow { flex: 1 1 120px; }
					.foot .hc-pill { white-space: nowrap; }
				</style>
				<div class="hc-card card ${big ? 'big' : ''}">
					<div class="view">
						${snap ? `<img class="under" src="${this.esc(snap)}" alt="" />` : ''}
						${showLive ? `<video muted autoplay playsinline></video><span class="tag live">Connecting…</span>` : `<span class="tag live" data-state="still">${snap ? `Still · ${this.esc(when(a.snapshot_at))}` : 'No still yet'}</span>`}
						${online ? '' : '<div class="off">Offline</div>'}
						<div class="tag seen"></div>
					</div>
					<div class="row foot">
						<div class="grow"><div class="name">${this.esc(h.devices.label(d))}</div><div class="hc-quiet line"></div></div>
						${light ? `<button class="hc-pill light">☀ Spotlight</button>` : ''}
						<button class="hc-pill open">Open ›</button>
					</div>
				</div>`;
			this.shadowRoot.querySelector('.open').onclick = () => h.ui.openDevice(d.device_id);
			if (showLive) this.play(a.stream);
		}
		// What changes often, without redrawing the video.
		this.shadowRoot.querySelector('.seen').innerHTML = sees
			.map((s) => `<span class="${s === 'Motion' ? 'motion' : ''}">${this.esc(s)}</span>`)
			.join('');
		this.shadowRoot.querySelector('.line').textContent = !online
			? 'Offline'
			: sees.length
				? `${sees[0]} now`
				: a.snapshot_at
					? `Quiet · newest still ${when(a.snapshot_at)}`
					: 'Quiet';
		const lb = this.shadowRoot.querySelector('.light');
		if (lb && light) {
			const on = light.attributes.on === true;
			lb.setAttribute('aria-pressed', String(on));
			lb.onclick = () => h.command(light.device_id, { on: !on });
		}
	}
}

// ---- the panel ----

/** A camera opened: live, its controls, its newest stills. */
class CameraPanel extends Screen {
	hd = false;
	stills = [];
	presets = [];
	#loaded = '';

	async load() {
		const h = this.host;
		const id = this.deviceId;
		if (!h || !id || this.#loaded === id) return;
		this.#loaded = id;
		const r = await h.devices.history(id, 'snapshot', { limit: 12 });
		if (r.ok) this.stills = r.data.filter((x) => typeof x.value === 'string').reverse();
		try {
			const res = await h.fetch(`/devices/${encodeURIComponent(id)}/schema`);
			const s = res.ok ? await res.json() : {};
			const acts = s.actions ?? s.schema?.actions ?? [];
			this._ptz = acts.some((x) => x.id === 'ptz_move');
			const go = acts.find((x) => x.id === 'goto_preset');
			const p = (go?.params ?? []).find((x) => x.name === 'preset');
			this.presets = (p?.options ?? []).map((o) => (typeof o === 'object' ? o : { value: o, label: o }));
		} catch {}
		this.drawn = '';
		this.draw();
	}

	draw() {
		const h = this.host;
		const d = this.device;
		if (!h || !d) return;
		void this.load();
		const a = d.attributes;
		const online = d.available !== false;
		const light = this.part('spotlight');
		const siren = this.part('siren');
		const stream = this.hd ? a.stream_hd : a.stream;
		// The frame is drawn once; what changes is set below, so the video plays on.
		const key = `${d.device_id}|${stream}|${online}|${this.hasPtz}|${this.presets.length}|${this.stills.length}|${!!light}|${!!siren}`;
		if (this.drawn !== key) {
			this.drawn = key;
			this.live?.stop();
			this.live = undefined;
			const snap = this.snapUrl(a);
			this.shadowRoot.innerHTML = `<style>${STYLE}
					.wrap { display: flex; flex-direction: column; gap: 14px; }
					.pad { position: absolute; z-index: 2; right: 12px; bottom: 12px; width: 108px; height: 108px; border-radius: 50%; background: rgba(0,0,0,.55); display: grid; grid-template: repeat(3, 1fr) / repeat(3, 1fr); place-items: center; }
					.pad button { all: unset; cursor: pointer; color: #fff; font-size: 16px; width: 32px; height: 32px; display: grid; place-items: center; border-radius: 50%; }
					.pad button:hover { background: rgba(255,255,255,.15); }
					.q { position: absolute; z-index: 2; right: 10px; top: 8px; display: flex; gap: 6px; }
					.chips { display: flex; flex-wrap: wrap; gap: 6px; }
					.chips span { padding: 3px 10px; border-radius: 999px; border: 1px solid var(--line); font-size: 12.5px; color: var(--ink-muted); }
					.chips span.on { background: var(--accent); border-color: var(--accent); color: var(--accent-ink, #1a1406); font-weight: 700; }
					.strip { display: flex; gap: 8px; overflow-x: auto; }
					.strip figure { margin: 0; flex: none; width: 140px; }
					.strip img { width: 140px; aspect-ratio: 16 / 9; object-fit: cover; border-radius: 8px; display: block; background: var(--surface-2); }
					.strip figcaption { font-size: 11.5px; color: var(--ink-muted); margin-top: 3px; }
					h3 { margin: 0 0 6px; font-size: 11.5px; letter-spacing: .1em; text-transform: uppercase; color: var(--ink-muted); }
					input[type=range] { flex: 1; }
				</style>
				<div class="wrap">
					<div class="view">
						${snap ? `<img class="under" src="${this.esc(snap)}" alt="" />` : ''}
						${online && stream ? `<video muted autoplay playsinline></video><span class="tag live">Connecting…</span>` : `<span class="tag live" data-state="still">${online ? (a.stream ? '' : 'No live video: go2rtc isn’t set') : 'Offline'}</span>`}
						${a.stream_hd ? `<div class="q"><button class="hc-pill sd" aria-pressed="${!this.hd}">SD</button><button class="hc-pill hd" aria-pressed="${this.hd}">HD</button></div>` : ''}
						${this.hasPtz ? `<div class="pad">
							<span></span><button data-dir="up" aria-label="Up">▲</button><span></span>
							<button data-dir="left" aria-label="Left">◀</button><span class="hc-quiet" style="font-size:10px;color:#ccd">PTZ</span><button data-dir="right" aria-label="Right">▶</button>
							<span></span><button data-dir="down" aria-label="Down">▼</button><span></span></div>` : ''}
					</div>
					<div><h3>It sees</h3><div class="chips sees"></div></div>
					${light ? `<div><h3>Spotlight</h3><div class="row"><button class="hc-pill lt"></button><input class="br" type="range" min="1" max="100" /><span class="hc-num brn"></span></div></div>` : ''}
					${siren ? `<div><h3>Siren</h3><div class="row"><button class="hc-pill sr">Sound the siren</button><span class="hc-quiet">Asks first.</span></div></div>` : ''}
					${this.presets.length ? `<div><h3>Go to</h3><div class="row" style="flex-wrap:wrap">${this.presets.map((p) => `<button class="hc-pill preset" data-id="${this.esc(p.value)}">${this.esc(p.label ?? p.value)}</button>`).join('')}</div></div>` : ''}
					${'ir_mode' in a ? `<div><h3>At night</h3><div class="row"><button class="hc-pill ir" data-v="auto">Infrared when dark</button><button class="hc-pill ir" data-v="off">No infrared</button></div></div>` : ''}
					<div><h3>Newest stills</h3><div class="strip">${
						this.stills.length
							? this.stills.map((s) => `<figure><img src="${this.esc(h.fileUrl?.(s.value) ?? '')}" alt="" loading="lazy" onerror="this.parentElement.remove()" /><figcaption>${this.esc(when(s.at))}</figcaption></figure>`).join('')
							: '<span class="hc-quiet">None yet: one is taken when it sees something.</span>'
					}</div></div>
					<div class="row"><button class="hc-pill snap">Take a still now</button><span class="hc-quiet">${this.esc(a.model ?? '')}</span></div>
				</div>`;
			const $ = (s) => this.shadowRoot.querySelector(s);
			$('.sd') && ($('.sd').onclick = () => { this.hd = false; this.draw(); });
			$('.hd') && ($('.hd').onclick = () => { this.hd = true; this.draw(); });
			for (const b of this.shadowRoot.querySelectorAll('.pad button')) b.onclick = () => h.command(d.device_id, { action: 'ptz_move', direction: b.dataset.dir, duration_ms: 400 });
			for (const b of this.shadowRoot.querySelectorAll('.preset')) b.onclick = () => h.command(d.device_id, { action: 'goto_preset', preset: Number(b.dataset.id) });
			for (const b of this.shadowRoot.querySelectorAll('.ir')) b.onclick = () => h.command(d.device_id, { ir_mode: b.dataset.v });
			$('.snap').onclick = () => h.command(d.device_id, { action: 'take_snapshot' });
			if (siren)
				$('.sr').onclick = async () => {
					if (await h.ui.confirm({ title: 'Sound the siren?', body: 'Everyone near the camera hears it.', go: 'Sound it', danger: true })) h.command(siren.device_id, { on: true });
				};
			if (light) {
				$('.lt').onclick = () => h.command(light.device_id, { on: light.attributes.on !== true });
				$('.br').onchange = (e) => h.command(light.device_id, { brightness_pct: Number(e.target.value) });
			}
			if (online && stream) this.play(stream);
		}
		// What changes often.
		this.shadowRoot.querySelector('.sees').innerHTML = DETECTS.filter(([k]) => k in a)
			.map(([k, label]) => `<span class="${a[k] ? 'on' : ''}">${label}</span>`)
			.join('');
		if (light) {
			const on = light.attributes.on === true;
			const lt = this.shadowRoot.querySelector('.lt');
			lt.textContent = on ? 'On' : 'Off';
			lt.setAttribute('aria-pressed', String(on));
			const br = this.shadowRoot.querySelector('.br');
			if (document.activeElement !== this && br) br.value = String(light.attributes.brightness_pct || 80);
			this.shadowRoot.querySelector('.brn').textContent = on ? `${light.attributes.brightness_pct}%` : '';
		}
		for (const b of this.shadowRoot.querySelectorAll('.ir')) b.setAttribute('aria-pressed', String(a.ir_mode === b.dataset.v));
	}

	get hasPtz() {
		return this.presets.length > 0 || this._ptz === true;
	}
}

customElements.define('hc-reolink-card', CameraCard);
customElements.define('hc-reolink-panel', CameraPanel);

// ---- several cameras, a card of its own (dashboard.card) ----

/** The most a card plays at once: past this, the rest show their newest still. */
const LIVE_AT_ONCE = 4;

/** The cameras a card shows: those its settings chose, in that order, else all by name. */
function chosen(devices, config, label) {
	const all = devices.filter((d) => d.device_type === 'camera');
	const ids = Array.isArray(config?.cameras) ? config.cameras : [];
	if (ids.length) return ids.map((id) => all.find((d) => d.device_id === id)).filter(Boolean);
	return all.sort((a, b) => label(a).localeCompare(label(b)));
}

/** "Cameras": several cameras, live at large, their newest stills otherwise; one tap opens one. */
class CameraWall extends Screen {
	#form = 'large';
	#config = {};
	#drawn = '';
	lives = new Map();

	set form(v) {
		this.#form = v;
		this.draw();
	}
	set config(v) {
		this.#config = v ?? {};
		this.draw();
	}

	disconnectedCallback() {
		super.disconnectedCallback();
		this.stopAll();
	}
	stopAll() {
		for (const l of this.lives.values()) l.stop();
		this.lives.clear();
	}

	draw() {
		const h = this.host;
		if (!h) return;
		const label = (d) => h.devices.label(d);
		const cams = chosen(this.devices, this.#config, label);
		const big = this.#form === 'large';
		const live = big && this.#config.live !== false;
		const shown = big ? cams : cams.slice(0, 4);
		const plays = (d, i) => live && i < LIVE_AT_ONCE && d.available !== false && d.attributes.stream;
		// Redrawn only when what is shown changes, so the videos keep playing.
		const key = JSON.stringify([this.#form, live, shown.map((d, i) => [d.device_id, !!plays(d, i)])]);
		if (key !== this.#drawn) {
			this.#drawn = key;
			this.stopAll();
			const title = this.#config.name || 'Cameras';
			this.shadowRoot.innerHTML = `<style>${STYLE}
					.wall { display: flex; flex-direction: column; gap: 10px; flex: 1; box-sizing: border-box; }
					.grid { display: grid; grid-template-columns: repeat(${shown.length > 1 ? 2 : 1}, minmax(0, 1fr)); gap: 8px; }
					.tile { all: unset; cursor: pointer; display: flex; flex-direction: column; gap: 4px; min-width: 0; }
					.tile:focus-visible .view { outline: 2px solid var(--accent, #f2b544); outline-offset: 2px; }
					.tile .name { font-size: 13px; }
					.none { color: var(--ink-muted, #9aa4b2); font-size: 13px; }
					[hidden] { display: none !important; }
				</style>
				<div class="hc-card wall">
					<span class="hc-tile-name">${this.esc(title)}</span>
					${
						shown.length
							? `<div class="grid">${shown
									.map(
										(d, i) => `<button class="tile" data-id="${this.esc(d.device_id)}">
											<div class="view">
												<img class="under" alt="" />
												${plays(d, i) ? '<video muted autoplay playsinline></video><span class="tag live">Connecting…</span>' : '<span class="tag live" data-state="still"></span>'}
												<div class="off" hidden>Offline</div>
												<div class="tag seen"></div>
											</div>
											<div class="name">${this.esc(label(d))}</div>
										</button>`
									)
									.join('')}</div>`
							: '<div class="none">No cameras yet. Add one in the plugin’s settings.</div>'
					}
				</div>`;
			for (const t of this.shadowRoot.querySelectorAll('.tile'))
				t.onclick = () => h.ui.openDevice(t.dataset.id);
			shown.forEach((d, i) => {
				if (!plays(d, i)) return;
				const tile = this.shadowRoot.querySelector(`.tile[data-id="${CSS.escape(d.device_id)}"]`);
				const tag = tile.querySelector('.live');
				this.lives.set(
					d.device_id,
					new Live(tile.querySelector('video'), d.attributes.stream, (s) => {
						tag.textContent = s === 'live' ? 'Live' : s === 'connecting' ? 'Connecting…' : s === 'unsupported' ? 'Stills only here' : 'Reconnecting…';
						tag.dataset.state = s;
					})
				);
			});
		}
		// What changes often, without touching the videos.
		for (const d of shown) {
			const tile = this.shadowRoot.querySelector(`.tile[data-id="${CSS.escape(d.device_id)}"]`);
			if (!tile) continue;
			const a = d.attributes;
			const snap = this.snapUrl(a);
			const img = tile.querySelector('img');
			if (snap && img.getAttribute('src') !== snap) img.setAttribute('src', snap);
			img.hidden = !snap;
			const still = tile.querySelector('.live[data-state=still]');
			if (still) still.textContent = snap ? `Still · ${when(a.snapshot_at)}` : 'No still yet';
			tile.querySelector('.off').hidden = d.available !== false;
			tile.querySelector('.seen').innerHTML = seeing(a)
				.map((s) => `<span class="${s === 'Motion' ? 'motion' : ''}">${this.esc(s)}</span>`)
				.join('');
		}
	}
}

/** The card's settings: its name, which cameras, and live or stills. */
class CameraWallSettings extends Screen {
	#config = {};
	#draft = null;

	set config(v) {
		this.#config = v ?? {};
		this.draw();
	}
	get cfg() {
		return this.#draft ?? { name: '', cameras: [], live: true, ...this.#config };
	}
	save(next) {
		this.#draft = next;
		this.dispatchEvent(new CustomEvent('change', { detail: { config: next } }));
		this.draw();
	}

	draw() {
		const h = this.host;
		if (!h) return;
		const label = (d) => h.devices.label(d);
		const all = this.devices.filter((d) => d.device_type === 'camera').sort((a, b) => label(a).localeCompare(label(b)));
		const cfg = this.cfg;
		const picked = cfg.cameras ?? [];
		const on = (id) => !picked.length || picked.includes(id);
		// Keep the name field as it is while someone types in it.
		const typing = this.shadowRoot.activeElement?.classList.contains('nm');
		if (!typing || !this.shadowRoot.querySelector('.nm')) {
			this.shadowRoot.innerHTML = `<style>${STYLE}
					.settings { display: flex; flex-direction: column; gap: 10px; }
					.check { display: flex; gap: 8px; align-items: center; }
				</style>
				<div class="settings">
					<label class="hc-field"><span class="hc-label">Name</span><input class="nm" placeholder="Cameras" /></label>
					<span class="hc-label">Cameras: all of them, or the ones you tick</span>
					${all.map((d) => `<label class="check"><input type="checkbox" class="cam" data-id="${this.esc(d.device_id)}" /> ${this.esc(label(d))}</label>`).join('')}
					<label class="check"><input type="checkbox" class="lv" /> Live video when it’s large (the first ${LIVE_AT_ONCE}; the rest show their newest still)</label>
				</div>`;
			const nm = this.shadowRoot.querySelector('.nm');
			nm.value = cfg.name ?? '';
			nm.onchange = () => this.save({ ...this.cfg, name: nm.value });
		}
		for (const c of this.shadowRoot.querySelectorAll('.cam')) {
			c.checked = on(c.dataset.id);
			c.onchange = () => {
				const now = this.cfg.cameras?.length ? this.cfg.cameras : all.map((d) => d.device_id);
				const next = now.includes(c.dataset.id) ? now.filter((x) => x !== c.dataset.id) : [...now, c.dataset.id];
				this.save({ ...this.cfg, cameras: next.length === all.length ? [] : next });
			};
		}
		const lv = this.shadowRoot.querySelector('.lv');
		lv.checked = cfg.live !== false;
		lv.onchange = () => this.save({ ...this.cfg, live: lv.checked });
	}
}

customElements.define('hc-reolink-cameras', CameraWall);
customElements.define('hc-reolink-cameras-settings', CameraWallSettings);
