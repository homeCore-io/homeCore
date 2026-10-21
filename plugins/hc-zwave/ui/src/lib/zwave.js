// What the Z-Wave screens share: the network's status in words, a lock's
// state and history, the streaming actions (add, remove, rebuild), all
// through the `host` each screen is handed. Pure where it can be, so the
// words are tested.

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

const attrs = (d) => d?.attributes ?? {};

/** "a", "a and b", "a, b and c". */
export function listWords(xs) {
	if (xs.length <= 1) return xs.join('');
	return `${xs.slice(0, -1).join(', ')} and ${xs.at(-1)}`;
}

/** "just now", "8 min ago", "8 h ago", "3 d ago". */
export function ago(iso, now = Date.now()) {
	const t = Date.parse(iso ?? '');
	if (!Number.isFinite(t)) return '';
	const s = Math.max(0, (now - t) / 1000);
	if (s < 60) return 'just now';
	if (s < 3600) return `${Math.round(s / 60)} min ago`;
	if (s < 86400) return `${Math.round(s / 3600)} h ago`;
	return `${Math.round(s / 86400)} d ago`;
}

/** "2:14 PM". */
export function clock(iso) {
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return '';
	return d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

/** "2:14 PM" today, "Yesterday", else "Oct 18". */
export function when(iso, now = Date.now()) {
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return '';
	const today = new Date(now);
	if (d.toDateString() === today.toDateString()) return clock(iso);
	const y = new Date(now - 86400e3);
	if (d.toDateString() === y.toDateString()) return 'Yesterday';
	return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

/** "today", "yesterday", "on Oct 18" — for "last used …". */
export function dayWords(iso, now = Date.now()) {
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return '';
	if (d.toDateString() === new Date(now).toDateString()) return 'today';
	if (d.toDateString() === new Date(now - 86400e3).toDateString()) return 'yesterday';
	return `on ${d.toLocaleDateString([], { month: 'short', day: 'numeric' })}`;
}

/** Where the plugin's own page and settings are, in the app. */
export const pageHref = (hc) => `/settings/plugins/${encodeURIComponent(hc.pluginId)}`;
export const settingsHref = (hc) => `${pageHref(hc)}?tab=settings`;

// ---------------------------------------------------------------- devices

const TYPE_WORDS = {
	lock: 'lock',
	switch: 'outlet',
	light: 'light',
	cover: 'cover',
	contact_sensor: 'door',
	motion_sensor: 'motion',
	water_sensor: 'leak sensor',
	smoke_sensor: 'smoke alarm',
	temperature_sensor: 'sensor',
	thermostat: 'thermostat',
	sensor: 'sensor',
	gateway: 'controller'
};

/** "Zooz ZEN15 · plug", "DB2 · lock". */
export function modelLine(node, device) {
	const kind = device?.device_type === 'switch' && attrs(device).power_w !== undefined ? 'plug' : TYPE_WORDS[device?.device_type] ?? '';
	const model = [node?.manufacturer, node?.model].filter(Boolean).join(' ');
	return [model, kind].filter(Boolean).join(' · ');
}

const watts = (w) => `${Math.round(w * 10) / 10} W`;

/** What it is doing now, in a few words: "Unlocked · door open", "On · 2.7 W". */
export function nowWords(device) {
	const a = attrs(device);
	if (device?.device_type === 'lock' || a.locked !== undefined) {
		const lk = a.jammed === true ? 'Jammed' : a.locked === true ? 'Locked' : a.locked === false ? 'Unlocked' : 'Not sure';
		return a.open === true ? `${lk} · door open` : lk;
	}
	if (a.on !== undefined) {
		const on = a.on ? 'On' : 'Off';
		return a.on && num(a.power_w) !== undefined ? `${on} · ${watts(num(a.power_w))}` : on;
	}
	if (a.open !== undefined) return a.open ? 'Open' : 'Closed';
	if (a.motion !== undefined) return a.motion ? 'Motion' : 'Clear';
	if (a.water_detected !== undefined) return a.water_detected ? 'Wet' : 'Dry';
	if (num(a.temperature) !== undefined) return `${num(a.temperature)} °C`;
	return '';
}

/** "Mains", "Battery 99% · sleeps", "Battery 29%". */
export function powerWords(node) {
	if (!node) return '';
	if (node.power === 'mains') return 'Mains';
	const b = num(node.battery_pct);
	const pct = b === undefined ? 'Battery' : `Battery ${b}%`;
	return node.sleeps ? `${pct} · sleeps` : pct;
}

/** The battery level that is worth a word: a lock's below 30 %, else 20 %. */
export function lowBattery(node, device) {
	const b = num(node?.battery_pct ?? attrs(device).battery_pct);
	if (attrs(device).battery_low === true) return true;
	if (b === undefined) return false;
	return b <= (device?.device_type === 'lock' ? 30 : 20);
}

export const SIGNAL_WORDS = { good: 'Good', fair: 'Fair', poor: 'Poor', unknown: '—' };
export const STATUS_WORDS = { alive: 'Answering', awake: 'Answering', asleep: 'Asleep', dead: 'Not answering', unknown: 'Not heard yet' };

/** The nodes that are devices (not the controller), with their homeCore device. */
export function rowsOf(status, devices) {
	const nodes = (status?.nodes ?? []).filter((n) => !n.is_controller);
	return nodes.map((n) => ({ node: n, device: devices.find((d) => d.device_id === n.device_id) }));
}

/** What needs someone, one line per device: not answering, battery, signal. */
export function attention(status, devices, label = (d) => d?.name) {
	const out = [];
	for (const { node, device } of rowsOf(status, devices)) {
		const name = device ? label(device) : node.name ?? `Node ${node.node_id}`;
		if (node.status === 'dead') out.push({ id: node.device_id, name, what: 'not answering', tone: 'bad' });
		else if (lowBattery(node, device)) out.push({ id: node.device_id, name, what: `battery ${num(node.battery_pct) ?? 'low'}${num(node.battery_pct) !== undefined ? '%' : ''}`, tone: 'bad' });
		else if (node.signal === 'poor' || node.signal === 'fair') out.push({ id: node.device_id, name, what: `signal ${node.signal}`, tone: 'warn' });
	}
	return out;
}

/** Devices homeCore has from this plugin that the network no longer does. */
export function gone(status, devices) {
	if (!status?.nodes || status.connection?.state !== 'connected') return [];
	const ids = new Set(status.nodes.map((n) => n.device_id));
	return devices.filter((d) => !ids.has(d.device_id));
}

/** "Straight to the controller", "Through node 12", "Through nodes 12 and 7". */
export function routeWords(route) {
	const r = route?.repeaters ?? [];
	if (!route) return 'Not known yet';
	if (!r.length) return 'Straight to the controller';
	return `Through node${r.length > 1 ? 's' : ''} ${listWords(r.map(String))}`;
}

/** "Mains · always listening", "Battery · wakes for commands", "Battery · sleeps, wakes every 12 h". */
export function powerDetail(node) {
	if (!node) return '';
	if (node.power === 'mains') return 'Mains · always listening';
	if (node.power === 'flirs') return 'Battery · wakes several times a second for commands';
	const w = num(node.wake_up_secs);
	if (!w) return 'Battery · sleeps';
	const every = w % 3600 === 0 ? `${w / 3600} h` : w % 60 === 0 ? `${w / 60} min` : `${w} s`;
	return `Battery · sleeps, wakes every ${every}`;
}

/** "S2 · encrypted", "S0 · encrypted", "Not encrypted". */
export function securityWords(sec) {
	if (!sec || sec === 'unknown') return 'Not known';
	if (sec === 'none') return 'Not encrypted';
	return `${sec} · encrypted`;
}

// ---------------------------------------------------------------- locks

export const METHOD_WORDS = { keypad: 'keypad', manual: 'by hand', rf: 'homeCore', auto: 'by itself' };

/** One lock event as a line of history: `{ what, how }`. */
export function eventWords(ev, names = {}, by = '') {
	if (!ev) return { what: '', how: '' };
	const who = ev.slot ? names[ev.slot] : '';
	const code = ev.slot ? `code ${ev.slot}${who ? ` · ${who}` : ''}` : '';
	let how = ev.method ? METHOD_WORDS[ev.method] ?? ev.method : '';
	if (ev.method === 'rf' && by) how = `homeCore · ${by}`;
	switch (ev.action) {
		case 'unlocked':
			return { what: code ? `Unlocked with ${code}` : 'Unlocked', how };
		case 'locked':
			return { what: code ? `Locked with ${code}` : 'Locked', how };
		case 'jammed':
			return { what: 'Jammed — the bolt didn’t move', how: ev.method === 'auto' ? 'by itself' : '' };
		case 'wrong_code':
			return { what: 'Wrong code entered', how };
		case 'code_added':
			return { what: code ? `Code ${ev.slot} added${who ? ` · ${who}` : ''}` : 'A code was added', how: 'at the lock' };
		case 'code_removed':
			return { what: code ? `Code ${ev.slot} removed` : 'A code was removed', how: '' };
		case 'codes_cleared':
			return { what: 'Every code removed', how: '' };
		case 'code_rejected':
			return { what: 'A code was refused (already in use)', how: '' };
		case 'keypad_disabled':
			return { what: 'Keypad switched off for a while', how: 'too many tries' };
		default:
			return { what: ev.action ?? '', how };
	}
}

/** The line under a lock's state: "Unlocked with code 2 · Cleaner · 2:14 PM". */
export function lastLine(device, names = {}) {
	const ev = attrs(device).last_lock_event;
	if (!ev || typeof ev !== 'object') return '';
	const { what, how } = eventWords(ev, names);
	const by = ev.method === 'auto' || ev.method === 'manual' ? how : '';
	return [what, by, clock(ev.at)].filter(Boolean).join(' · ');
}

/** A card's short line: "Code 2 · Cleaner · 2:14 PM · battery 29%", "Locked · by itself 1:02 PM". */
export function cardLine(device, names = {}) {
	const a = attrs(device);
	const ev = a.last_lock_event;
	const parts = [];
	if (ev && typeof ev === 'object') {
		if (ev.slot) parts.push(`Code ${ev.slot}${names[ev.slot] ? ` · ${names[ev.slot]}` : ''}`);
		else if (ev.method) parts.push(METHOD_WORDS[ev.method] ?? ev.method);
		if (ev.at) parts.push(clock(ev.at));
	}
	if (num(a.battery_pct) !== undefined) parts.push(`battery ${num(a.battery_pct)}%`);
	return parts.join(' · ');
}

/** "Locked · by itself 1:02 PM" — the compact card's one line. */
export function compactLine(device) {
	const a = attrs(device);
	const ev = a.last_lock_event;
	const state = a.jammed === true ? 'Jammed' : a.locked === true ? 'Locked' : a.locked === false ? 'Unlocked' : 'Not sure';
	if (!ev || typeof ev !== 'object' || !ev.at) return state;
	const how = ev.slot ? `code ${ev.slot}` : ev.method ? METHOD_WORDS[ev.method] ?? '' : '';
	return `${state} · ${[how, clock(ev.at)].filter(Boolean).join(' ')}`;
}

/** The "Lock again after" choices, in seconds; 0 is never. */
export const RELOCK_CHOICES = [
	[0, 'Never'],
	[30, '30 seconds'],
	[60, '1 minute'],
	[300, '5 minutes']
];

/** How long until it locks again by itself, in seconds (0: it doesn't). */
export function relockOf(device) {
	const a = attrs(device);
	if (num(a.lock_auto_relock_secs) !== undefined) return num(a.lock_auto_relock_secs);
	if (num(a.lock_operation_type) === 2) return num(a.lock_timeout_secs) ?? 0;
	return 0;
}

/** The command that sets it: v4 locks' own auto-relock, else timed mode. */
export function relockBody(device, secs) {
	const a = attrs(device);
	if (a.lock_auto_relock_secs !== undefined) return { lock_auto_relock_secs: secs };
	return secs > 0 ? { lock_operation_type: 2, lock_timeout_secs: secs } : { lock_operation_type: 1 };
}

/** "In use · last used 2:14 PM", "In use · last used yesterday", "Turned off · kept on the lock". */
export function slotLine(slot, lastUsed, now = Date.now()) {
	if (slot.status === 'disabled') return 'Turned off · kept on the lock';
	if (slot.status !== 'in_use') return '';
	if (!lastUsed) return 'In use';
	const day = dayWords(lastUsed, now);
	return `In use · last used ${day === 'today' ? clock(lastUsed) : day}`;
}

/** History lines from device_state_changed events (newest first). */
export function historyFrom(rows, names = {}) {
	const out = [];
	const seen = new Set();
	for (const r of rows) {
		const e = r?.event ?? r;
		const ev = e?.current?.last_lock_event;
		if (!ev || !(e.changed ?? []).includes('last_lock_event')) continue;
		const key = `${ev.at}|${ev.action}`;
		if (seen.has(key)) continue;
		seen.add(key);
		const by = e.change?.actor_name ?? '';
		out.push({ at: ev.at, slot: ev.slot ?? null, ...eventWords(ev, names, by) });
	}
	return out.sort((x, y) => String(y.at).localeCompare(String(x.at)));
}

/** When each slot was last used, from history: `{ slot: iso }`. */
export function lastUsedBySlot(history) {
	const out = {};
	for (const h of history) if (h.slot && !out[h.slot]) out[h.slot] = h.at;
	return out;
}

// ---------------------------------------------------------------- actions

/** A reply that came back as data but says it failed. */
export function failed(r) {
	if (!r) return 'No answer';
	if (!r.ok) return r.error;
	if (r.data?.status === 'error') return r.data.error ?? 'It failed';
	return '';
}

/**
 * Follow a streaming action's events (`/plugins/{id}/command/{rid}/stream`),
 * calling `on` with each. Returns the way to stop.
 */
export function follow(hc, requestId, on) {
	const ctl = new AbortController();
	(async () => {
		let res;
		try {
			res = await hc.fetch(`/plugins/${encodeURIComponent(hc.pluginId)}/command/${encodeURIComponent(requestId)}/stream`, {
				signal: ctl.signal,
				headers: { accept: 'text/event-stream' }
			});
		} catch {
			if (!ctl.signal.aborted) on({ stage: 'error', message: 'homeCore is not answering' });
			return;
		}
		if (!res.ok || !res.body) {
			on({ stage: 'error', message: `homeCore said ${res.status}` });
			return;
		}
		const reader = res.body.getReader();
		const dec = new TextDecoder();
		let buf = '';
		const seen = new Set();
		for (;;) {
			let chunk;
			try {
				chunk = await reader.read();
			} catch {
				return;
			}
			if (chunk.done) return;
			buf += dec.decode(chunk.value, { stream: true });
			let at;
			while ((at = buf.indexOf('\n\n')) >= 0) {
				const block = buf.slice(0, at);
				buf = buf.slice(at + 2);
				for (const ev of parseSse(block)) {
					const key = `${ev.stage}:${ev.ts ?? ''}:${ev.label ?? ''}:${ev.percent ?? ''}`;
					if (seen.has(key)) continue;
					seen.add(key);
					on(ev);
					if (['complete', 'error', 'canceled', 'timeout'].includes(ev.stage)) {
						ctl.abort();
						return;
					}
				}
			}
		}
	})();
	return () => ctl.abort();
}

/** The events in one SSE block. */
export function parseSse(block) {
	const data = block
		.split('\n')
		.filter((l) => l.startsWith('data:'))
		.map((l) => l.slice(5).replace(/^ /, ''))
		.join('\n');
	if (!data) return [];
	try {
		return [JSON.parse(data)];
	} catch {
		return [];
	}
}

/** Start a streaming action; its request id, or an error. */
export async function start(hc, id, params = {}) {
	const r = await hc.action(id, params);
	const err = failed(r);
	if (err) return { error: err };
	const rid = r.data?.request_id;
	return rid ? { rid } : { error: 'It did not start' };
}

export const respond = (hc, rid, response) => hc.action('respond', { target_request_id: rid, response });
export const cancel = (hc, rid) => hc.action('cancel', { target_request_id: rid });

/** Read a controller backup in pieces and hand it to the browser to save. */
export async function downloadBackup(hc, onProgress = () => {}) {
	const r = await hc.action('backup_nvm');
	const err = failed(r);
	if (err) return err;
	const { backup_id, chunks, file_name } = r.data;
	const parts = [];
	for (let i = 0; i < chunks; i++) {
		onProgress(i, chunks);
		const c = await hc.action('backup_nvm_chunk', { backup_id, index: i });
		const e = failed(c);
		if (e) return e;
		const bin = atob(c.data.data);
		const bytes = new Uint8Array(bin.length);
		for (let j = 0; j < bin.length; j++) bytes[j] = bin.charCodeAt(j);
		parts.push(bytes);
	}
	const blob = new Blob(parts, { type: 'application/octet-stream' });
	const url = URL.createObjectURL(blob);
	const a = document.createElement('a');
	a.href = url;
	a.download = file_name || 'zwave-nvm.bin';
	document.body.appendChild(a);
	a.click();
	a.remove();
	setTimeout(() => URL.revokeObjectURL(url), 10000);
	return '';
}

/** The house's rooms, by name. */
export async function roomsOf(hc) {
	try {
		const r = await hc.fetch('/areas');
		if (!r.ok) return [];
		const list = await r.json();
		return (Array.isArray(list) ? list : list.areas ?? []).map((a) => a.name).filter(Boolean).sort((a, b) => a.localeCompare(b));
	} catch {
		return [];
	}
}

/**
 * The automations that used a device that is gone, turned off — what
 * homeCore does itself when a device is deleted (and, for a device that left
 * the network, may already have done). Answers which are off now and which
 * are still on: `{ off: [names], on: [names] }`.
 */
export async function turnOffAutomationsUsing(hc, deviceId) {
	const quoted = JSON.stringify(deviceId);
	const using = async () => {
		const r = await hc.fetch('/automations');
		if (!r.ok) return [];
		const body = await r.json();
		const rules = Array.isArray(body) ? body : (body.rules ?? []);
		return rules.filter((x) => JSON.stringify(x).includes(quoted));
	};
	try {
		for (const rule of await using()) {
			if (rule.enabled === false) continue;
			await hc.fetch(`/automations/${encodeURIComponent(rule.id)}`, {
				method: 'PATCH',
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({ enabled: false })
			});
		}
		const now = await using();
		return { off: now.filter((x) => x.enabled === false).map((x) => x.name), on: now.filter((x) => x.enabled !== false).map((x) => x.name) };
	} catch {
		return { off: [], on: [] };
	}
}

/** Slot names from a lock's schema (`last_lock_slot` options): `{ slot: name }`. */
export async function slotNames(hc, deviceId) {
	const r = await hc.query('list_user_codes', { device_id: deviceId });
	if (failed(r)) return { names: {}, slots: [] };
	const names = {};
	for (const s of r.data.slots ?? []) if (s.name) names[s.slot] = s.name;
	return { names, slots: r.data.slots ?? [] };
}
