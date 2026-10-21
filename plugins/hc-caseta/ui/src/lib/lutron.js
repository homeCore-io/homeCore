// Copied verbatim from hc-lutron/ui/src/lib/lutron.js — change both together (src/lib/shared.test.js checks).
// What the screens share: a keypad's buttons and lights, presses from
// history, the automations on a button, and words for all of them.

/** Give a shadow root the app's parts (host.theme.sheet) beside its own. */
export function adopt(el, host) {
	const root = el?.shadowRoot;
	if (!root || !host) return;
	const sheet = host.theme.sheet;
	if (!root.adoptedStyleSheets.includes(sheet)) root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
}

export const num = (v) => {
	const n = typeof v === 'string' && v.trim() !== '' ? Number(v) : v;
	return typeof n === 'number' && Number.isFinite(n) ? n : undefined;
};

/** A device's buttons, `[{number, name}]`, as it reports them. */
export function buttonsOf(d) {
	const list = d?.attributes?.available_buttons;
	return Array.isArray(list)
		? list.filter((b) => num(b?.number) !== undefined).map((b) => ({ number: num(b.number), name: String(b.name ?? `Button ${b.number}`) }))
		: [];
}

/** A button nobody named reads "Button 5". */
export const unnamed = (b) => /^Button \d+$/.test(b?.name ?? '');

/** A seeTouch's raise and lower rockers: 16–19. */
export const isRocker = (n) => n >= 16 && n <= 19;

/** The rows of a drawn keypad: its buttons, then one rocker pair. */
export function faceOf(buttons) {
	const keys = buttons.filter((b) => !isRocker(b.number));
	const bottom = buttons.filter((b) => b.number === 18 || b.number === 19);
	const top = buttons.filter((b) => b.number === 16 || b.number === 17);
	const pair = (bottom.length ? bottom : top).sort((a, b) => a.number - b.number);
	return { keys, lower: pair.find((b) => b.number % 2 === 0), raise: pair.find((b) => b.number % 2 === 1) };
}

/** A button's light, 0–3, or undefined when it has none (or none reported). */
export const ledOf = (d, n) => num(d?.attributes?.[`led_${n}`]);

export const LED_STATES = [
	{ value: 0, name: 'Off', word: 'off' },
	{ value: 1, name: 'On', word: 'on' },
	{ value: 2, name: 'Slow flash', word: 'slow flash' },
	{ value: 3, name: 'Fast flash', word: 'fast flash' }
];

/** "pressed", "held", "double-tapped". */
export const pressWord = (v) => ({ hold: 'held', double_click: 'double-tapped' })[v] ?? 'pressed';

/** "just now", "3 min ago", "2 h ago", "4 d ago", from an ISO time. */
export function ago(iso) {
	const t = Date.parse(iso ?? '');
	if (!Number.isFinite(t)) return '';
	const s = Math.max(0, (Date.now() - t) / 1000);
	if (s < 60) return 'just now';
	if (s < 3600) return `${Math.round(s / 60)} min ago`;
	if (s < 86400) return `${Math.round(s / 3600)} h ago`;
	return `${Math.round(s / 86400)} d ago`;
}

/**
 * The presses of a device's buttons, newest first, folded from each
 * `button_N`'s history: a press, a press that turned into a hold, a
 * double-tap. A release only ends one.
 */
export function foldPresses(buttons, histories) {
	const out = [];
	buttons.forEach((b, i) => {
		const rows = [...(histories[i] ?? [])].sort((x, y) => x.at.localeCompare(y.at));
		let last = null;
		for (const r of rows) {
			if (r.value === 'press' || r.value === 'double_click') {
				last = { at: r.at, number: b.number, name: b.name, how: pressWord(r.value) };
				out.push(last);
			} else if (r.value === 'hold' && last && Date.parse(r.at) - Date.parse(last.at) < 10_000) {
				last.how = 'held';
			} else if (r.value === 'hold') {
				out.push({ at: r.at, number: b.number, name: b.name, how: 'held' });
			}
		}
	});
	return out.sort((x, y) => y.at.localeCompare(x.at));
}

/** Read the presses of `buttons` on `deviceId` from homeCore's history. */
export async function presses(hc, deviceId, buttons, limit = 40) {
	if (typeof hc?.devices?.history !== 'function' || !buttons.length) return [];
	const got = await Promise.all(buttons.map((b) => hc.devices.history(deviceId, `button_${b.number}`, { limit })));
	return foldPresses(
		buttons,
		got.map((r) => (r.ok ? r.data : []))
	);
}

/** When an attribute last changed to its present value, from history. */
export async function since(hc, deviceId, attribute) {
	if (typeof hc?.devices?.history !== 'function') return null;
	const r = await hc.devices.history(deviceId, attribute, { limit: 50 });
	return r.ok && r.data.length ? r.data.at(-1).at : null;
}

/** Every `kind` trigger (`DeviceStateChanged`, `ButtonEvent`) in a rule, wherever it sits. */
function triggersOf(rule, kind = 'DeviceStateChanged') {
	const found = [];
	const walk = (v) => {
		if (!v || typeof v !== 'object') return;
		if (Array.isArray(v)) return v.forEach(walk);
		if (v[kind]) found.push(v[kind]);
		for (const [k, x] of Object.entries(v)) if (k === 'trigger' || k === 'triggers' || k === 'Any' || k === 'any') walk(x);
	};
	walk(rule.trigger);
	walk(rule.triggers);
	return found;
}

/** The automations that start when `button` on `deviceId` is used: on its
 * `button_N`, or a button trigger for it (or for any of its buttons). */
export function automationsOn(rules, deviceId, button) {
	return rules.filter(
		(r) =>
			triggersOf(r).some(
				(t) =>
					t.attribute === `button_${button}` &&
					(t.device_id === deviceId || (Array.isArray(t.device_ids) && t.device_ids.includes(deviceId)))
			) || triggersOf(r, 'ButtonEvent').some((t) => t.device_id === deviceId && (t.button_number == null || Number(t.button_number) === button))
	);
}

/** The house's automations, for anyone who may read them. */
export async function loadRules(hc) {
	if (!hc?.me?.can('automations:read') || typeof hc.fetch !== 'function') return [];
	try {
		const r = await hc.fetch('/automations');
		if (!r.ok) return [];
		const body = await r.json();
		return Array.isArray(body) ? body : (body.rules ?? []);
	} catch {
		return [];
	}
}

/** A device's declared attributes, from its schema (cached per load). */
const schemas = new Map();
export async function schemaOf(hc, deviceId) {
	if (schemas.has(deviceId)) return schemas.get(deviceId);
	if (typeof hc?.fetch !== 'function') return null;
	const p = hc
		.fetch(`/devices/${encodeURIComponent(deviceId)}/schema`)
		.then((r) => (r.ok ? r.json() : null))
		.catch(() => null);
	schemas.set(deviceId, p);
	return p;
}

/** The buttons a schema says have a light: `led_N`. */
export const ledButtonsIn = (schema) =>
	Object.keys(schema?.attributes ?? {})
		.map((k) => /^led_(\d+)$/.exec(k)?.[1])
		.filter(Boolean)
		.map(Number);

/** A visor receiver's contact inputs, `cci_N_open`, from its schema and state. */
export function inputsOf(d, schema) {
	const keys = new Set([...Object.keys(schema?.attributes ?? {}), ...Object.keys(d?.attributes ?? {})]);
	return [...keys]
		.map((k) => /^cci_(\d+)_open$/.exec(k))
		.filter(Boolean)
		.map((m) => Number(m[1]))
		.sort((a, b) => a - b)
		.map((c) => ({ component: c, attribute: `cci_${c}_open`, name: inputName(c) }));
}

/** RA2's visor receiver numbers its two contact inputs 32 and 33. */
export const inputName = (c) => (c === 32 ? 'Input 1' : c === 33 ? 'Input 2' : `Input ${c}`);

/** A scene that says whether it is on: its phantom button has a light. */
export const reportsState = (s) => s?.attributes?.led_component !== undefined;
export const sceneOn = (s) => reportsState(s) && s.attributes.active === true;

/** Scenes in groups by their room, the ones in no room first as "House". */
export function sceneGroups(devices) {
	const scenes = devices.filter((d) => d.device_type === 'scene');
	const by = new Map();
	for (const s of scenes) {
		const g = s.room ?? 'House';
		if (!by.has(g)) by.set(g, []);
		by.get(g).push(s);
	}
	const order = (s) => num(s.attributes.phantom_button) ?? 1000;
	return [...by.entries()]
		.sort(([a], [b]) => (a === 'House' ? -1 : b === 'House' ? 1 : a.localeCompare(b)))
		.map(([name, list]) => ({ name, scenes: list.sort((x, y) => order(x) - order(y) || x.name.localeCompare(y.name)) }));
}

/** "Kitchen 6 Button's buttons 5 and 6", for the page's note. */
export function listWords(items) {
	if (items.length <= 1) return items.join('');
	return `${items.slice(0, -1).join(', ')} and ${items.at(-1)}`;
}

/** Where the plugin's settings are, in the app. */
export const settingsHref = (hc) => `/settings/plugins/${encodeURIComponent(hc.pluginId)}?tab=settings`;
