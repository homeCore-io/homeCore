// The template plugin's own screens, as plain custom elements — the smallest
// complete example of a plugin's UI. The web app imports this module, creates
// the elements its manifest names, and sets their properties:
//
//   hc-template-page     (plugin.page)   host
//   hc-template-card     (device.card)   host, deviceId, form
//   hc-template-bulb     (look.drawing)  host, thing, deviceId
//   hc-template-header   (look.piece)    host, room
//   hc-template-symbols  (look.symbol)   host, name
//
// The last three are for the looks (Settings › Look): once a look chooses
// them, they draw any device in the house, not only the plugin's. They draw
// from what they're given and never command a device: a drawing asks for a
// tap with an `act` event and the app does it.
//
// `host` is everything a screen may use (see core/sdk/ui/index.d.ts): the
// plugin's devices live, commands, its actions, the app's confirm and look.
// Each element draws into its own shadow root and adopts the app's parts
// stylesheet, so it looks like the app in light and dark.
//
// Screens read and write homeCore's names — `brightness_pct`, 0–100 — never
// the device's own scale: the plugin converted at the edge already.

/** Shared by both elements: a shadow root with the app's look, and re-drawing
 *  whenever a property or one of the plugin's devices changes. */
class Screen extends HTMLElement {
	#host;
	#stop;
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
		// Called now, and again on every change to the plugin's devices.
		this.#stop = h.devices.subscribe((list) => {
			this.devices = list;
			this.draw();
		});
	}
	get host() {
		return this.#host;
	}

	disconnectedCallback() {
		this.#stop?.();
		this.#stop = undefined;
	}

	/** Text made safe to put in markup. */
	esc(s) {
		return String(s ?? '').replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
	}

	draw() {}
}

/** The plugin's page: every light, with a switch each. */
class TemplatePage extends Screen {
	draw() {
		const h = this.host;
		const rows = this.devices
			.map(
				(d) => `<tr>
					<td>${this.esc(h.devices.label(d))}</td>
					<td class="hc-num">${d.attributes.on ? `${d.attributes.brightness_pct ?? '—'}%` : 'off'}</td>
					<td><button class="hc-pill" data-id="${this.esc(d.device_id)}">${d.attributes.on ? 'Turn off' : 'Turn on'}</button></td>
				</tr>`
			)
			.join('');
		this.shadowRoot.innerHTML = `
			<section class="hc-card">
				<div class="hc-head"><h2>Lights</h2><span class="hc-quiet">${this.devices.length}</span></div>
				<table class="hc-table">
					<thead><tr><th>Light</th><th>Brightness</th><th></th></tr></thead>
					<tbody>${rows}</tbody>
				</table>
				<p class="hc-quiet">This page comes with the plugin — see ui/src/index.js.</p>
			</section>`;
		for (const b of this.shadowRoot.querySelectorAll('button[data-id]')) {
			const d = this.devices.find((x) => x.device_id === b.dataset.id);
			// A declared action: `{ action: 'toggle' }`. The card's switch below
			// writes the attribute instead — both shapes reach the plugin.
			b.onclick = () => h.command(d.device_id, { action: 'toggle' });
		}
	}
}

/** A light's card on Home and the rooms, at any of the three sizes. */
class TemplateCard extends Screen {
	#deviceId;
	#form = 'standard';
	set deviceId(v) {
		this.#deviceId = v;
		this.draw();
	}
	set form(v) {
		this.#form = v;
		this.draw();
	}

	draw() {
		const h = this.host;
		const d = this.devices.find((x) => x.device_id === this.#deviceId);
		if (!h || !d) return;
		const on = d.attributes.on === true;
		const small = this.#form === 'compact';
		this.shadowRoot.innerHTML = `
			<style>
				.top { display: flex; align-items: center; justify-content: space-between; gap: 10px; }
				.name { all: unset; cursor: pointer; display: flex; flex-direction: column; min-width: 0; }
				input[type=range] { width: 100%; }
			</style>
			<div class="hc-tile ${small ? 'compact' : ''} ${on ? 'lit' : ''}">
				<div class="top">
					<button class="name"><span class="hc-tile-name">${this.esc(h.devices.label(d))}</span>
						<span class="hc-tile-state">${on ? `On · ${d.attributes.brightness_pct ?? ''}%` : 'Off'}</span></button>
					<button class="hc-pill ${on ? 'go' : ''}" data-act="toggle">${on ? 'On' : 'Off'}</button>
				</div>
				${small ? '' : `<input type="range" min="0" max="100" value="${d.attributes.brightness_pct ?? 0}" aria-label="Brightness">`}
			</div>`;
		const root = this.shadowRoot;
		root.querySelector('.name').onclick = () => h.ui.openDevice(d.device_id);
		root.querySelector('[data-act=toggle]').onclick = () => h.command(d.device_id, { on: !on });
		const range = root.querySelector('input[type=range]');
		if (range) range.onchange = () => h.command(d.device_id, { brightness_pct: Number(range.value) });
	}
}

/**
 * A look's drawing for lights (look.drawing): a bulb that fills to its level
 * in the light's own color. Given `thing` (its values: name, on, level,
 * color, state, …), set afresh as the light changes. A tap on the bulb asks
 * the app to switch it, a tap on the name to open it.
 */
class TemplateBulb extends HTMLElement {
	#thing;
	constructor() {
		super();
		this.attachShadow({ mode: 'open' });
	}
	set host(h) {
		this.shadowRoot.adoptedStyleSheets = [h.theme.sheet];
	}
	set thing(t) {
		this.#thing = t;
		this.draw();
	}
	act(what) {
		this.dispatchEvent(new CustomEvent('act', { detail: { do: what } }));
	}
	draw() {
		const t = this.#thing;
		if (!t) return;
		const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
		const level = t.on ? (t.level ?? 100) : 0;
		this.shadowRoot.innerHTML = `
			<style>
				.bulb-card { display: flex; align-items: center; gap: 14px; }
				.bulb { all: unset; cursor: pointer; position: relative; flex: none;
					width: 44px; height: 44px; border-radius: 50%; overflow: hidden;
					background: var(--surface-2); box-shadow: 0 0 ${level / 4}px ${esc(t.color)}; }
				.bulb span { position: absolute; inset: auto 0 0 0; height: ${level}%;
					background: ${esc(t.color)}; transition: height .4s; }
				.name { all: unset; cursor: pointer; display: flex; flex-direction: column; min-width: 0; }
			</style>
			<div class="hc-tile ${t.on ? 'lit' : ''} bulb-card">
				<button class="bulb" aria-label="${t.on ? 'Turn off' : 'Turn on'}"><span></span></button>
				<button class="name"><span class="hc-tile-name">${esc(t.name)}</span>
					<span class="hc-tile-state">${esc(t.state)}</span></button>
			</div>`;
		this.shadowRoot.querySelector('.bulb').onclick = () => this.act('switch');
		this.shadowRoot.querySelector('.name').onclick = () => this.act('open');
	}
}

/**
 * A look's piece (look.piece, `header`): a room's header as one line — its
 * name, and how many of its lights are on. Given `room`: `{ id, name, things }`.
 */
class TemplateHeader extends HTMLElement {
	constructor() {
		super();
		this.attachShadow({ mode: 'open' });
	}
	set host(h) {
		this.shadowRoot.adoptedStyleSheets = [h.theme.sheet];
	}
	set room(r) {
		const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
		const lit = r.things.filter((t) => t.kind === 'light' && t.on).length;
		this.shadowRoot.innerHTML = `
			<div class="hc-head"><h1 style="margin: 0">${esc(r.name)}</h1>
				<span class="hc-tag">${lit ? `${lit} on` : 'All off'}</span></div>`;
	}
}

/**
 * A look's symbols (look.symbol): each named in the manifest, drawn when the
 * app sets `name`, at the size of its text, in its color.
 */
const SYMBOLS = {
	bulb: '<path d="M9 18h6M10 21h4M12 3a6 6 0 0 0-4 10.5c.7.7 1 1.5 1 2.5h6c0-1 .3-1.8 1-2.5A6 6 0 0 0 12 3z"/>',
	plug: '<path d="M9 2v6M15 2v6M7 8h10v4a5 5 0 0 1-10 0zM12 17v5"/>'
};
class TemplateSymbols extends HTMLElement {
	set host(_h) {}
	set name(n) {
		this.innerHTML = `<svg viewBox="0 0 24 24" width="1em" height="1em" fill="none" stroke="currentColor"
			stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${SYMBOLS[n] ?? ''}</svg>`;
	}
}

customElements.define('hc-template-page', TemplatePage);
customElements.define('hc-template-card', TemplateCard);
customElements.define('hc-template-bulb', TemplateBulb);
customElements.define('hc-template-header', TemplateHeader);
customElements.define('hc-template-symbols', TemplateSymbols);
