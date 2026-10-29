// What the YoLink screens share: words for its devices, a lock's history,
// batteries in quarters, the water at a glance — all through the `host` each
// screen is handed. Pure where it can be, so the words are tested.

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
	if (d.toDateString() === new Date(now).toDateString()) return clock(iso);
	if (d.toDateString() === new Date(now - 86400e3).toDateString()) return 'Yesterday';
	return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

/** "Oct 18" — a date with no time, for "last shook Oct 14". */
export function day(iso, now = Date.now()) {
	const d = new Date(iso);
	if (Number.isNaN(d.getTime())) return '';
	if (d.toDateString() === new Date(now).toDateString()) return 'today';
	if (d.toDateString() === new Date(now - 86400e3).toDateString()) return 'yesterday';
	return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

/** Where the plugin's own page and settings are, in the app. */
export const pageHref = (hc) => `/settings/plugins/${encodeURIComponent(hc.pluginId)}`;
export const settingsHref = (hc) => `${pageHref(hc)}?tab=settings`;
/** Where the house's alert rules — and their "Do something…" — are. */
export const ALERT_RULES_HREF = '/settings/alerts';

/** A reply that came back as data but says it failed. */
export function failed(r) {
	if (!r) return 'No answer';
	if (!r.ok) return r.error;
	if (r.data?.status === 'error') return r.data.error ?? 'It failed';
	return '';
}

// ---------------------------------------------------------------- units

/** A temperature in °C as the person reads it: "−14.3 °C", "6.3 °F". */
export function temp(hc, c) {
	const v = num(c);
	if (v === undefined) return '—';
	const shown = hc?.units?.show?.(v, '°C');
	const text = shown?.text ?? `${Math.round(v * 10) / 10} °C`;
	return text.replace(/^-/, '−');
}

/** The number a person reads for `c` °C (for an input). */
export function tempNumber(hc, c) {
	const v = num(c);
	if (v === undefined) return undefined;
	const shown = hc?.units?.show?.(v, '°C');
	return shown ? shown.value : Math.round(v * 10) / 10;
}

/** The person's temperature unit: "°F" or "°C". */
export const tempUnit = (hc) => hc?.units?.show?.(0, '°C')?.unit ?? '°C';

/** What the person typed, in °C. */
export function toCelsius(hc, v) {
	const n = num(v);
	if (n === undefined) return undefined;
	return hc?.units?.toCore ? Math.round(hc.units.toCore(n, '°C') * 100) / 100 : n;
}

// ---------------------------------------------------------------- devices

export const isLock = (d) => d?.device_type === 'lock' || attrs(d).locked !== undefined;
export const isLeak = (d) => d?.device_type === 'water_sensor' || attrs(d).water_detected !== undefined;
export const isTemp = (d) => d?.device_type === 'temperature_sensor';
export const isDoor = (d) => d?.device_type === 'contact_sensor';
export const isRelay = (d) => d?.device_type === 'switch';
export const isVibration = (d) => d?.device_type === 'vibration_sensor';
/** A temperature probe — a freezer, a fridge — with no humidity. */
export const isProbe = (d) => isTemp(d) && attrs(d).humidity === undefined && attrs(d).temperature !== undefined;

const watts = (w) => `${Math.round(w * 10) / 10} W`;

/** A switch's "On · 3.7 W", "Off". */
export function switchWords(d) {
	const a = attrs(d);
	if (a.on === undefined) return '—';
	const w = num(a.power_w);
	return a.on ? (w !== undefined ? `On · ${watts(w)}` : 'On') : 'Off';
}

// ---------------------------------------------------------------- batteries

/** YoLink's 0–4 level, from `battery_pct` (its quarters × 25). */
export function quarters(d) {
	const p = num(attrs(d).battery_pct);
	return p === undefined ? undefined : Math.max(0, Math.min(4, Math.round(p / 25)));
}

/** A level in words. Whether it wants changing is homeCore's to say — `batteryLow`. */
export const BATTERY_WORDS = ['Empty', 'A quarter', 'Half', 'Three quarters', 'Full'];

/** "full", "2 of 4", "1 of 4 · low". */
export function batteryShort(d) {
	const q = quarters(d);
	if (q === undefined) return '—';
	if (q === 4) return 'full';
	return `${q} of 4`;
}

/** Whether a device's battery is low: what homeCore's alerts say
 *  (`host.alerts`), else — on an app without them — the plugin's flag or
 *  YoLink's own "1 of 4". */
export function batteryLow(d, alerts) {
	if (!d) return false;
	if (alerts) return alerts.some((x) => x.device_id === d.device_id && x.kind === 'low_battery');
	return attrs(d).battery_low === true || (quarters(d) ?? 4) <= 1;
}

/** Devices with less than a full battery, lowest first. */
export function batteries(devices) {
	return devices
		.filter((d) => quarters(d) !== undefined && quarters(d) < 4)
		.sort((x, y) => quarters(x) - quarters(y) || x.name.localeCompare(y.name));
}

// ---------------------------------------------------------------- reporting

/** Reporting: it is there and has checked in. */
export const reporting = (d) => d?.available !== false;

/** "Dry · checked in 12 min ago", "Wet · since 7:40 PM", "Not reporting · last 9 h ago". */
export function leakLine(d, now = Date.now()) {
	const a = attrs(d);
	const seen = a.last_seen ? ago(a.last_seen, now) : '';
	if (!reporting(d)) return `Not reporting${seen ? ` · last ${seen}` : ''}`;
	if (a.water_detected === true) return `Wet${d.wetSince ? ` · since ${clock(d.wetSince)}` : ''}`;
	return `Dry${seen ? ` · checked in ${seen}` : ''}`;
}

/** The water at a glance: `{ total, wet, quiet, checkedIn }`. */
export function water(devices, now = Date.now(), withinMs = 3600e3) {
	const leaks = devices.filter(isLeak);
	const wet = leaks.filter((d) => attrs(d).water_detected === true);
	const quiet = leaks.filter((d) => !reporting(d));
	const checkedIn = leaks.filter((d) => {
		const t = Date.parse(attrs(d).last_seen ?? '');
		return reporting(d) && Number.isFinite(t) && now - t <= withinMs;
	});
	return { leaks, total: leaks.length, wet, quiet, checkedIn };
}

/** The automations that start on a leak sensor of these getting wet: enabled
 * ones whose trigger reads `water_detected` on one of `ids`. */
export function leakAutomations(rules, ids) {
	const set = new Set(ids);
	return (rules ?? []).filter((r) => {
		if (r.enabled === false) return false;
		const t = JSON.stringify(r.trigger ?? r.triggers ?? '');
		if (!t.includes('"water_detected"')) return false;
		return [...set].some((id) => t.includes(JSON.stringify(id)));
	});
}

// ---------------------------------------------------------------- locks

export const METHOD_WORDS = {
	fingerprint: 'fingerprint',
	code: 'code on the keypad',
	card: 'card',
	key: 'key',
	button: 'button on the lock',
	app: 'from the app',
	homecore: 'homeCore',
	auto: 'by itself',
	remote: 'remotely'
};

export const ACTION_WORDS = {
	locked: 'Locked',
	unlocked: 'Unlocked',
	lock_failed: 'Couldn’t lock',
	unlock_failed: 'Couldn’t unlock',
	jammed: 'Jammed'
};

const BAD = new Set(['lock_failed', 'unlock_failed', 'jammed']);

/** "Locked", "Unlocked", "Not sure". */
export function lockState(d) {
	const a = attrs(d);
	return a.locked === true ? 'Locked' : a.locked === false ? 'Unlocked' : 'Not sure';
}

/** "by fingerprint", "from the app", "by itself", "homeCore". */
export function byWords(method) {
	if (!method) return '';
	const w = METHOD_WORDS[method] ?? method;
	if (['fingerprint', 'key', 'card', 'code', 'homecore'].includes(method)) return `by ${w}`;
	return w;
}

/** The line under a lock's state: "Unlocked by fingerprint · 8:05 AM". */
export function lastLine(d, now = Date.now()) {
	const ev = attrs(d).last_lock_event;
	if (!ev || typeof ev !== 'object') return '';
	const what = ACTION_WORDS[ev.action] ?? ev.action ?? '';
	return [`${what}${ev.method ? ` ${byWords(ev.method)}` : ''}`, when(ev.at, now)].filter(Boolean).join(' · ');
}

/** A card's line: "By itself · 2:14 PM". */
export function cardLine(d, now = Date.now()) {
	const ev = attrs(d).last_lock_event;
	if (!ev || typeof ev !== 'object') return '';
	const how = ev.method ? byWords(ev.method) : '';
	const head = how ? how.charAt(0).toUpperCase() + how.slice(1) : ACTION_WORDS[ev.action] ?? '';
	return [head, when(ev.at, now)].filter(Boolean).join(' · ');
}

/** The lock's last problem: `{ what, line }` — "Couldn't unlock", "Oct 18 · from the app". */
export function lastProblem(d, now = Date.now()) {
	const a = attrs(d);
	if (!a.last_alert) return null;
	const k = String(a.last_alert).toLowerCase();
	const what = k.includes('unlockfailed')
		? 'Couldn’t unlock'
		: k.includes('lockfailed')
			? 'Couldn’t lock'
			: k.includes('jam')
				? 'Jammed'
				: k.includes('door') || k.includes('remind')
					? 'Door left open'
					: String(a.last_alert);
	const source = a.last_alert_source ? METHOD_WORDS[a.last_alert_source] ?? a.last_alert_source : '';
	const at = a.last_alert_at ? when(a.last_alert_at, now) : '';
	return { what, line: [at, source].filter(Boolean).join(' · ') || 'when, YoLink didn’t say' };
}

/** "Locks itself after 10 s", "Door closed · battery low". */
export function lockSubline(d, alerts) {
	const a = attrs(d);
	const parts = [];
	if (a.open === true) parts.push('Door open');
	else if (a.open === false) parts.push('Door closed');
	const auto = num(a.auto_lock_secs);
	if (auto) parts.push(`Locks itself after ${secsWords(auto)}`);
	if (batteryLow(d, alerts)) parts.push('battery low');
	const s = parts.join(' · ');
	return s.charAt(0).toUpperCase() + s.slice(1);
}

/** "10 s", "1 min", "5 min". */
export function secsWords(s) {
	if (s % 60 === 0 && s >= 60) return `${s / 60} min`;
	return `${s} s`;
}

/** "Lock again after" choices, in seconds; 0 is never. */
export const AUTO_LOCK = [
	[0, 'Never'],
	[10, '10 seconds'],
	[30, '30 seconds'],
	[60, '1 minute'],
	[300, '5 minutes']
];
export const autoChoices = (secs) => (AUTO_LOCK.some(([s]) => s === secs) || secs === undefined ? AUTO_LOCK : [...AUTO_LOCK, [secs, `${secs} seconds`]].sort((a, b) => a[0] - b[0]));

/** Beep volume, YoLink's 0–3. */
export const SOUND = [
	[0, 'Off'],
	[1, 'Low'],
	[2, 'Medium'],
	[3, 'High']
];

/**
 * A lock's history, newest first: `last_lock_event` values (kept by
 * homeCore) and the door opening (`open` going true), with who asked — an
 * automation, a person — from the event log where it still has them.
 */
export function historyFrom({ lockEvents = [], doorOpened = [], log = [] } = {}) {
	const by = new Map();
	for (const r of log) {
		const e = r?.event ?? r;
		const ev = e?.current?.last_lock_event;
		if (ev?.at && e.change?.actor_name) by.set(ev.at, e.change.actor_name);
	}
	const out = [];
	const seen = new Set();
	for (const ev of lockEvents) {
		if (!ev || typeof ev !== 'object' || !ev.at) continue;
		const key = `${ev.at}|${ev.action}`;
		if (seen.has(key)) continue;
		seen.add(key);
		let how = ev.method ? (ev.method === 'homecore' ? 'homeCore' : byWords(ev.method)) : '';
		if (ev.method === 'homecore' && by.get(ev.at)) how = `homeCore · ${by.get(ev.at)}`;
		out.push({ at: ev.at, what: ACTION_WORDS[ev.action] ?? ev.action, how, bad: BAD.has(ev.action) });
	}
	for (const at of doorOpened) out.push({ at, what: 'Door opened', how: '', bad: false });
	return out.sort((x, y) => String(y.at).localeCompare(String(x.at)));
}

/** From a history of a boolean, the times it went true. */
export function becameTrue(points) {
	const out = [];
	let before;
	for (const p of points ?? []) {
		if (p.value === true && before !== true) out.push(p.at);
		before = p.value;
	}
	return out;
}

/** From a history of `water_detected`: the last wet spell, `{ from, to }`. */
export function lastWet(points) {
	let from = null;
	let spell = null;
	for (const p of points ?? []) {
		if (p.value === true && !from) from = p.at;
		if (p.value === false && from) {
			spell = { from, to: p.at };
			from = null;
		}
	}
	return from ? { from, to: null } : spell;
}

/** "for 4 min", "for 2 h". */
export function lasted(from, to) {
	const s = (Date.parse(to) - Date.parse(from)) / 1000;
	if (!Number.isFinite(s) || s < 0) return '';
	if (s < 90) return 'for a minute';
	if (s < 5400) return `for ${Math.round(s / 60)} min`;
	return `for ${Math.round(s / 3600)} h`;
}

// ---------------------------------------------------------------- status

/** One device's row from the plugin's `status`. */
export const statusOf = (status, id) => (status?.devices ?? []).find((x) => x.device_id === id);

/** "Good", "Fair", "Poor", "—" — with dBm. */
export const SIGNAL_WORDS = { good: 'Good', fair: 'Fair', poor: 'Poor', unknown: '—' };

/** "Local hub YS1606 · live events · last event 2 min ago". */
export function hubLine(status, now = Date.now()) {
	if (!status?.configured) return 'Not set up yet — open Settings';
	const model = (status.hub?.model ?? '').replace(/-.*$/, '');
	const where = status.mode === 'cloud' ? 'YoLink cloud' : `Local hub${model ? ` ${model}` : ''}`;
	const feed = status.feed?.connected ? 'live events' : 'live events are down — reconnecting';
	const last = status.last_event_at ? `last event ${ago(status.last_event_at, now)}` : '';
	return [where, feed, last].filter(Boolean).join(' · ');
}

/** How long before a quiet battery sensor shows as not reporting: "9 h". */
export function quietAfter(status) {
	const m = num(status?.silent_after_mins);
	if (!m) return '';
	return m % 60 === 0 ? `${m / 60} h` : `${m} min`;
}
