// The template plugin's own screens, as plain custom elements — the smallest
// complete example of a plugin's UI. The web app imports this module, creates
// the elements its manifest names, and sets their properties:
//
//   hc-template-page  (plugin.page)  host
//   hc-template-card  (device.card)  host, deviceId, form
//
// `host` is everything a screen may use (see core/sdk/ui/index.d.ts): the
// plugin's devices live, commands, its actions, the app's confirm and look.
// Each element draws into its own shadow root and adopts the app's parts
// stylesheet, so it looks like the app in light and dark.

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
					<td class="hc-num">${d.attributes.on ? `${d.attributes.brightness ?? '—'}` : 'off'}</td>
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
			b.onclick = () => h.command(d.device_id, { on: !d.attributes.on });
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
						<span class="hc-tile-state">${on ? `On · ${d.attributes.brightness ?? ''}` : 'Off'}</span></button>
					<button class="hc-pill ${on ? 'go' : ''}" data-act="toggle">${on ? 'On' : 'Off'}</button>
				</div>
				${small ? '' : `<input type="range" min="0" max="255" value="${d.attributes.brightness ?? 0}" aria-label="Brightness">`}
			</div>`;
		const root = this.shadowRoot;
		root.querySelector('.name').onclick = () => h.ui.openDevice(d.device_id);
		root.querySelector('[data-act=toggle]').onclick = () => h.command(d.device_id, { on: !on });
		const range = root.querySelector('input[type=range]');
		if (range) range.onchange = () => h.command(d.device_id, { brightness: Number(range.value) });
	}
}

customElements.define('hc-template-page', TemplatePage);
customElements.define('hc-template-card', TemplateCard);
