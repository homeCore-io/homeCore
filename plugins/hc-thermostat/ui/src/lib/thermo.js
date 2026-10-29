// What the thermostat screens share: the thermostats, their temperatures in
// the person's unit, what each is doing in words, and the house's sensors and
// plugs — all from what the plugin publishes and homeCore's API, through the
// `host` each screen is handed. Pure where it can be, so it can be tested.

/** Give a shadow root the app's parts (host.theme.sheet) beside its own. */
export function adopt(el, host) {
	const root = el?.shadowRoot;
	if (!root || !host) return;
	const sheet = host.theme.sheet;
	if (!root.adoptedStyleSheets.includes(sheet))
		root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
}

export const num = (v) => {
	const n = typeof v === 'string' && v.trim() !== '' ? Number(v) : v;
	return typeof n === 'number' && Number.isFinite(n) ? n : undefined;
};

export const attrs = (d) => d?.attributes ?? {};

/** The plugin's thermostats, by name. */
export function thermostatsOf(devices, label = (d) => d.name) {
	return devices
		.filter((d) => d.device_type === 'thermostat')
		.sort((a, b) => label(a).localeCompare(label(b)));
}

/** A card's shorter name: "Master Bedroom AC Thermostat" → "Master Bedroom AC". */
export const shortName = (name) => String(name ?? '').replace(/\s+thermostat$/i, '') || name;

// ── Kinds and modes ─────────────────────────────────────────────────────────

export const KINDS = {
	ac: { name: 'AC', does: 'cools', a: 'an AC', the: 'the AC', modes: ['cool', 'off'] },
	heater: {
		name: 'Heater',
		does: 'heats',
		a: 'a heater',
		the: 'the heater',
		modes: ['heat', 'off']
	},
	fan: { name: 'Fan', does: 'cools', a: 'a fan', the: 'the fan', modes: ['cool', 'off'] }
};
export const MODE_NAMES = { cool: 'Cool', heat: 'Heat', off: 'Off' };

/** Its kind: what it says, or — for one from before kinds — its mode's. */
export function kindOf(d) {
	const a = attrs(d);
	if (KINDS[a.kind]) return a.kind;
	return a.hvac_mode === 'heat' ? 'heater' : 'ac';
}
export const kindWords = (d) => KINDS[kindOf(d)];
/** The mode it runs in: cool, or heat for a heater. */
export const activeMode = (d) => kindWords(d).modes[0];
/** "An AC only cools, so Heat isn't offered." */
export function onlyNote(d) {
	const k = kindOf(d);
	if (k === 'heater') return "A heater only heats, so Cool isn't offered.";
	if (k === 'fan') return "A fan only runs when it's too warm, so Heat isn't offered.";
	return "An AC only cools, so Heat isn't offered.";
}

// ── Temperatures in the person's unit ───────────────────────────────────────

/** °F when the person reads °F (host.units), else °C. */
export const isF = (units) => units?.temperature === 'F';
export const cToF = (c) => (c * 9) / 5 + 32;
export const fToC = (f) => ((f - 32) * 5) / 9;

const tidy = (v, places) => {
	const k = 10 ** places;
	return Math.round(v * k) / k || 0;
};

/** A reading: "70.7°" (one place). Absent: "—". */
export function tempText(c, units) {
	const v = num(c);
	if (v === undefined) return '—';
	return `${tidy(isF(units) ? cToF(v) : v, 1)}°`;
}

/** A target: "68°" in °F (whole degrees), "20.5°" in °C (half degrees). */
export function targetText(c, units) {
	const v = num(c);
	if (v === undefined) return '—';
	return isF(units) ? `${Math.round(cToF(v))}°` : `${tidy(Math.round(v * 2) / 2, 1)}°`;
}

/** A difference (the swing): "0.9°" in °F, "0.5°" in °C. */
export function diffText(dc, units) {
	const v = num(dc) ?? 0;
	return `${tidy(isF(units) ? (v * 9) / 5 : v, 1)}°`;
}

/** The unit's letter, for a sentence that needs it: "°F". */
export const unitLetter = (units) => (isF(units) ? '°F' : '°C');

/**
 * The next target up or down — half a degree Celsius at a time (what the
 * thermostat keeps), skipping a step that would read the same in °F.
 */
export function nextTarget(c, dir, units) {
	let v = Math.round((num(c) ?? 20) * 2) / 2;
	const shown = targetText(v, units);
	for (let i = 0; i < 4; i++) {
		v += dir * 0.5;
		if (targetText(v, units) !== shown) break;
	}
	return Math.min(45, Math.max(-40, v));
}

/** Targets around one, half a degree Celsius apart, each read differently. */
export function targetsAround(c, units, below = 6, above = 6) {
	const centre = Math.round((num(c) ?? 20) * 2) / 2;
	const out = [];
	const seen = new Set();
	for (let i = -below; i <= above; i++) {
		const v = centre + i * 0.5;
		const t = targetText(v, units);
		if (seen.has(t)) continue;
		seen.add(t);
		out.push({ value: v, text: t });
	}
	return out;
}

// ── Time ────────────────────────────────────────────────────────────────────

const ms = (iso) => {
	const t = Date.parse(iso ?? '');
	return Number.isFinite(t) ? t : undefined;
};

/** "2 h 14 m", "52 m", "1 m 48 s", "40 s". */
export function span(secs) {
	const s = Math.max(0, Math.round(num(secs) ?? 0));
	if (s >= 3600) {
		const m = Math.floor((s % 3600) / 60);
		return `${Math.floor(s / 3600)} h${m ? ` ${String(m).padStart(2, '0')} m` : ''}`;
	}
	if (s >= 60) {
		const r = s % 60;
		return s >= 600 || !r ? `${Math.floor(s / 60)} m` : `${Math.floor(s / 60)} m ${r} s`;
	}
	return `${s} s`;
}

/** A guard or limit, said plainly: "3 min", "4 h", "90 min". */
export function minutes(secs) {
	const s = num(secs) ?? 0;
	if (s >= 3600 && s % 3600 === 0) return `${s / 3600} h`;
	if (s >= 60) return `${Math.round(s / 60)} min`;
	return `${s} s`;
}

/** "4 min ago", "just now", "2 h 14 m ago", "8 days ago". */
export function ago(iso, now = Date.now()) {
	const t = ms(iso);
	if (t === undefined) return '';
	const s = (now - t) / 1000;
	if (s < 60) return 'just now';
	if (s < 3600) return `${Math.round(s / 60)} min ago`;
	if (s >= 2 * 86400) return `${Math.floor(s / 86400)} days ago`;
	return `${span(s)} ago`;
}

/** "3:12" left, from an ISO time. */
export function countdown(iso, now = Date.now()) {
	const t = ms(iso);
	if (t === undefined) return '';
	const s = Math.max(0, Math.round((t - now) / 1000));
	return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

/** A time of day as the person's locale says it: "14:02" / "2:02 PM". */
export function clock(iso) {
	const t = ms(iso);
	if (t === undefined) return '';
	return new Date(t).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

/** Local midnight before `now`, as ISO. */
export function midnight(now = Date.now()) {
	const d = new Date(now);
	d.setHours(0, 0, 0, 0);
	return d.toISOString();
}

// ── Today ───────────────────────────────────────────────────────────────────

/** Today's run time, counting a run going on now. */
export function ranToday(d, now = Date.now()) {
	const a = attrs(d);
	let s = num(a.run_secs_today) ?? 0;
	const since = ms(a.on_since);
	if (a.actuator_state === true && since !== undefined) {
		s += Math.max(0, (now - Math.max(since, Date.parse(midnight(now)))) / 1000);
	}
	return s;
}

/** "ran 3 h 12 m · 9 times" — or "hasn't run today". */
export function todayLine(d, now = Date.now()) {
	const a = attrs(d);
	const n = num(a.cycles_today) ?? 0;
	const s = ranToday(d, now);
	if (!n && s < 60) return "hasn't run today";
	return `ran ${span(s)} · ${n} time${n === 1 ? '' : 's'}`;
}

// ── What it is doing ────────────────────────────────────────────────────────

/** Its readings, by sensor: value, when, and whether it is current. */
export function readingsOf(d) {
	const r = attrs(d).sensor_readings;
	const ids = Array.isArray(attrs(d).sensor_ids) ? attrs(d).sensor_ids : Object.keys(r ?? {});
	return ids.map((id) => ({
		id,
		temperature: num(r?.[id]?.temperature),
		at: r?.[id]?.at ?? null,
		stale: r?.[id]?.stale !== false,
		online: r?.[id]?.online !== false
	}));
}

/** The newest reading, current or not. */
export function newest(d) {
	return readingsOf(d)
		.filter((r) => r.at)
		.sort((x, y) => ms(y.at) - ms(x.at))[0];
}

/** Its target now — the hold's while one runs. */
export const targetNow = (d) => num(attrs(d).hold_setpoint) ?? num(attrs(d).setpoint);

/** The band it keeps: on past one edge, off past the other. */
export function band(d) {
	const a = attrs(d);
	const t = targetNow(d) ?? 20;
	const half = (num(a.hysteresis) ?? 1) / 2;
	return { target: t, half, low: t - half, high: t + half };
}

/** Whose words: names of other devices, from the house (see `house`). */
const called = (names, id) => names?.[id] ?? id;

/** Statuses a card shows as "needs you". */
export const NEEDS_YOU = [
	'sensors_stale',
	'actuator_not_responding',
	'override',
	'max_run_exceeded'
];
export const needsYou = (d) => d?.available === false || NEEDS_YOU.includes(attrs(d).reason);

/**
 * What it is doing, for a person: `{ code, title, why, tone, act }`. `tone`
 * is cool | heat | quiet | warn | plain; `act` is the button that helps,
 * `{ label, kind }` with kind start_now | resume | resync | alert.
 */
export function nowOf(d, { units, names = {}, now = Date.now() } = {}) {
	const a = attrs(d);
	const k = kindWords(d);
	const room = tempText(a.temperature, units);
	const b = band(d);
	const cooling = activeMode(d) === 'cool';
	const sw = called(names, a.actuator_device_id);
	const word = cooling ? 'cooling' : 'heating';
	if (d?.available === false)
		return {
			code: 'offline',
			title: 'Not running',
			why: 'The thermostat plugin is not running, so nothing is being controlled.',
			tone: 'warn'
		};
	switch (a.reason) {
		case 'off':
			return {
				code: 'off',
				title: 'Off',
				why: `The thermostat is off and leaves ${k.the} alone. Turning it to ${MODE_NAMES[activeMode(d)]} takes over again.`,
				tone: 'quiet'
			};
		case 'cooling':
		case 'heating':
			return {
				code: a.reason,
				title: a.reason === 'cooling' ? 'Cooling' : 'Heating',
				why: cooling
					? `The room is ${room}, above ${tempText(b.high, units)} (the target plus the swing). It stops at ${tempText(b.low, units)}.`
					: `The room is ${room}, below ${tempText(b.low, units)} (the target less the swing). It stops at ${tempText(b.high, units)}.`,
				tone: cooling ? 'cool' : 'heat'
			};
		case 'waiting_min_off': {
			const off = a.actuator_last_change ? span((now - ms(a.actuator_last_change)) / 1000) : '';
			return {
				code: a.reason,
				title: `Waiting ${countdown(a.lockout_until, now)}`,
				why: `The room is ${room} and wants ${word}, but ${k.the} went off ${off} ago and stays off at least ${minutes(a.min_off_secs)}${kindOf(d) === 'ac' ? ' to protect the compressor' : ''}.`,
				tone: 'quiet',
				act: { label: 'Start now anyway', kind: 'start_now' }
			};
		}
		case 'waiting_min_on':
			return {
				code: a.reason,
				title: `Stopping in ${countdown(a.lockout_until, now)}`,
				why: `The room is ${room} — far enough — but once on, ${k.the} stays on at least ${minutes(a.min_on_secs)}.`,
				tone: 'quiet',
				act: { label: 'Stop now anyway', kind: 'start_now' }
			};
		case 'override': {
			const by = a.actuator_last_change ? ` at ${clock(a.actuator_last_change)}` : '';
			return {
				code: a.reason,
				title: `Left alone until ${clock(a.override_until)}`,
				why: `Someone turned ${k.the} ${a.actuator_state ? 'on' : 'off'} by hand${by}. The thermostat takes over again at ${clock(a.override_until)}, or when the target or mode changes.`,
				tone: 'warn',
				act: { label: 'Take over now', kind: 'resume' }
			};
		}
		case 'sensors_stale': {
			const last = newest(d);
			const quiet = last ? span((now - ms(last.at)) / 1000) : '';
			const offline = readingsOf(d).length > 0 && readingsOf(d).every((r) => !r.online);
			const hold = a.on_stale === 'hold';
			const did = hold ? `${k.the} is left as it is` : `${k.the} was turned off`;
			const lead = hold ? 'No reading' : 'Off — no reading';
			return {
				code: a.reason,
				title: offline
					? `${hold ? 'Its' : 'Off — its'} sensor is offline`
					: last
						? `${lead} for ${quiet}`
						: `${lead} yet`,
				why: last
					? `${called(names, last.id)} last reported at ${clock(last.at)}. With no current reading ${did} and an alert raised. It starts again on the next reading.`
					: `None of its sensors has reported. Until one does, ${did} and an alert is up.`,
				tone: 'warn',
				act: { label: 'See the alert', kind: 'alert' }
			};
		}
		case 'actuator_not_responding':
			return {
				code: a.reason,
				title: a.actuator_online === false ? `${sw} is offline` : `${sw} isn't answering`,
				why:
					a.actuator_online === false
						? `It can't be told anything until it is back; it is asked again then.`
						: `It hasn't confirmed turning ${a.actuator_state ? 'off' : 'on'}. It is asked again, less often each time, and an alert is up.`,
				tone: 'warn',
				act: { label: 'Send it again', kind: 'resync' }
			};
		case 'max_run_exceeded':
			return {
				code: a.reason,
				title: 'Off — ran its longest run',
				why: `It ran ${minutes(a.max_run_secs)} without a break and was turned off. It starts again once the room reaches its target.`,
				tone: 'warn',
				act: { label: 'Start it again', kind: 'resume' }
			};
		case 'no_switch':
			return {
				code: a.reason,
				title: 'No switch chosen',
				why: 'Choose the plug or switch it runs, under Switches.',
				tone: 'warn'
			};
		default: {
			const t = num(a.temperature);
			const where =
				t === undefined
					? ''
					: t < b.low
						? `below ${targetText(b.target, units)}`
						: t > b.high
							? `above ${targetText(b.target, units)}`
							: `within ${targetText(b.target, units)} ± ${diffText(b.half, units)}`;
			const nothing =
				(cooling && t !== undefined && t < b.low) || (!cooling && t !== undefined && t > b.high)
					? ` — nothing to ${cooling ? 'cool' : 'heat'}`
					: '';
			return {
				code: 'idle_in_band',
				title: 'Idle',
				why:
					t === undefined ? 'Waiting for a reading.' : `The room is ${room}, ${where}${nothing}.`,
				tone: 'plain'
			};
		}
	}
}

/** The short "doing now" line on a card: "Cooling to 68°", "Off — sensor quiet 2 h". */
export function cardLine(d, opts = {}) {
	const a = attrs(d);
	const n = nowOf(d, opts);
	const t = targetText(targetNow(d), opts.units);
	switch (n.code) {
		case 'cooling':
			return `Cooling to ${t}`;
		case 'heating':
			return `Heating to ${t}`;
		case 'idle_in_band':
			return `Idle · ${MODE_NAMES[a.hvac_mode] ?? ''} to ${t}`;
		case 'sensors_stale': {
			const last = newest(d);
			const s = last ? ((opts.now ?? Date.now()) - ms(last.at)) / 1000 : 0;
			return last
				? `Off — sensor quiet ${s >= 3600 ? `${Math.floor(s / 3600)} h` : span(s)}`
				: 'Off — no reading';
		}
		case 'override':
			return n.title;
		case 'waiting_min_off':
			return `Waiting ${countdown(a.lockout_until, opts.now)} to ${activeMode(d) === 'cool' ? 'cool' : 'heat'}`;
		default:
			return n.title;
	}
}

/** The plugin page's "doing now" line: "Cooling — 70.7° is above 68.9°". */
export function pageLine(d, opts = {}) {
	const a = attrs(d);
	const { units } = opts;
	const n = nowOf(d, opts);
	const b = band(d);
	const room = tempText(a.temperature, units);
	const t = num(a.temperature);
	switch (n.code) {
		case 'cooling':
			return `Cooling — ${room} is above ${tempText(b.high, units)}`;
		case 'heating':
			return `Heating — ${room} is below ${tempText(b.low, units)}`;
		case 'idle_in_band':
			if (t === undefined) return 'Idle — waiting for a reading';
			if (t < b.low) return `Idle — ${room} is below ${targetText(b.target, units)}`;
			if (t > b.high) return `Idle — ${room} is above ${targetText(b.target, units)}`;
			return `Idle — ${room} is within ${targetText(b.target, units)} ± ${diffText(b.half, units)}`;
		case 'sensors_stale': {
			const last = newest(d);
			const lead = a.on_stale === 'hold' ? 'No reading' : 'Off';
			return last
				? `${lead} — its sensor has been quiet for ${span(((opts.now ?? Date.now()) - ms(last.at)) / 1000)}`
				: n.title;
		}
		default:
			return n.title;
	}
}

/** The sentence under a "needs you" card. */
export function needsLine(d, opts = {}) {
	const a = attrs(d);
	const k = kindWords(d);
	const n = nowOf(d, opts);
	if (n.code === 'sensors_stale') {
		const last = newest(d);
		return last
			? `${called(opts.names, last.id)} hasn't reported since ${clock(last.at)}, so ${k.the} was turned off.`
			: n.why;
	}
	if (n.code === 'override')
		return `Switched by hand — the thermostat takes over at ${clock(a.override_until)}.`;
	return n.why;
}

// ── Holds ───────────────────────────────────────────────────────────────────

/** Seconds from now to the next `hh:00` local. */
export function secsUntil(hour, now = Date.now()) {
	const d = new Date(now);
	d.setHours(hour, 0, 0, 0);
	if (d.getTime() <= now) d.setDate(d.getDate() + 1);
	return Math.round((d.getTime() - now) / 1000);
}

/**
 * The one-tap holds, around the usual target: a degree Celsius lower for a
 * nap or a night (cooler for an AC, cheaper for a heater) and a degree higher
 * for a hot afternoon away.
 */
export function holdChips(d, units, now = Date.now()) {
	const t = num(attrs(d).setpoint) ?? 20;
	const lower = Math.round((t - 1) * 2) / 2;
	const higher = Math.round((t + 1) * 2) / 2;
	return [
		{ id: 'h1', setpoint: lower, for_secs: 3600, name: `${targetText(lower, units)} for 1 h` },
		{ id: 'h2', setpoint: lower, for_secs: 7200, name: `${targetText(lower, units)} for 2 h` },
		{ id: 'h4', setpoint: higher, for_secs: 14400, name: `${targetText(higher, units)} for 4 h` },
		{
			id: 'night',
			setpoint: lower,
			for_secs: secsUntil(7, now),
			name: `${targetText(lower, units)} until ${clock(new Date(now + secsUntil(7, now) * 1000).toISOString())}`
		}
	];
}

/** "For a nap, …" or "Holding 66° until 15:30 — then back to 68°." */
export function holdSaid(d, units) {
	const a = attrs(d);
	if (num(a.hold_setpoint) === undefined)
		return 'For a nap, a hot afternoon, a night. Times of day come from automations.';
	return `Holding ${targetText(a.hold_setpoint, units)} until ${clock(a.hold_until)} — then back to ${targetText(a.setpoint, units)}.`;
}

// ── The house: names, rooms, sensors and plugs ──────────────────────────────

/** Whether a device can be a thermostat's switch: a plug or switch — an
 * `on`, and not a light, a lock, a thermostat or a software flag. */
const NOT_SWITCHES = new Set([
	'light',
	'dimmer',
	'lock',
	'thermostat',
	'media_player',
	'scene',
	'cover',
	'timer',
	'virtual_switch',
	'fan_light',
	'group'
]);
export function isSwitch(dev) {
	const a = dev?.attributes ?? {};
	if (typeof a.on !== 'boolean') return false;
	if (NOT_SWITCHES.has(dev.device_type)) return false;
	if ('locked' in a || 'brightness_pct' in a || 'brightness' in a) return false;
	if (String(dev.plugin_id ?? '').startsWith('core.')) return false;
	return true;
}

/** Whether a device reads a room's temperature (not a thermostat). */
export function isSensor(dev) {
	const a = dev?.attributes ?? {};
	return (
		dev?.device_type !== 'thermostat' &&
		num(a.temperature) !== undefined &&
		!String(dev?.device_id ?? '').startsWith('thermostat_')
	);
}

/** A house device's name, as the house shows it. */
export const nameOf = (dev) => dev?.name_override || dev?.name || dev?.device_id || '';

/**
 * Plugs and sensors for the Add flow and "Change…": each with its room, and
 * — for plugs — which thermostat already runs it.
 */
export function choices(house, rooms, thermostats, label = (d) => d.name) {
	const roomOf = (dev) => {
		const key = dev.area_override ?? dev.area ?? dev.room;
		const r = rooms.find((x) => x.id === key || x.slug === key || x.name === key);
		return r?.name ?? (typeof key === 'string' && key ? key : '');
	};
	const runs = {};
	for (const t of thermostats) {
		const id = attrs(t).actuator_device_id;
		if (id) runs[id] = label(t);
	}
	const by = (x, y) => (x.room || '~').localeCompare(y.room || '~') || x.name.localeCompare(y.name);
	return {
		plugs: house
			.filter(isSwitch)
			.map((dev) => ({
				id: dev.device_id,
				name: nameOf(dev),
				room: roomOf(dev),
				usedBy: runs[dev.device_id] ?? null,
				on: dev.attributes?.on === true,
				available: dev.available !== false
			}))
			.sort(by),
		sensors: house
			.filter(isSensor)
			.map((dev) => ({
				id: dev.device_id,
				name: nameOf(dev),
				room: roomOf(dev),
				temperature: num(dev.attributes?.temperature),
				at: dev.last_seen ?? null,
				available: dev.available !== false
			}))
			.sort(by)
	};
}

// One copy of the house for every screen, read again after a while.
let cache = null;
/** Every device and room in the house (as the viewer), kept 30 s. */
export async function house(hc, { fresh = false } = {}) {
	if (!fresh && cache && Date.now() - cache.at < 30000) return cache;
	try {
		const [dr, rr] = await Promise.all([hc.fetch('/devices'), hc.fetch('/areas')]);
		const devices = dr.ok ? await dr.json() : [];
		const rooms = rr.ok ? await rr.json() : [];
		cache = { at: Date.now(), devices, rooms, names: namesOf(devices) };
	} catch {
		cache = cache ?? { at: 0, devices: [], rooms: [], names: {} };
	}
	return cache;
}

export function namesOf(devices) {
	return Object.fromEntries(devices.map((x) => [x.device_id, nameOf(x)]));
}

/** Where the plugin's page and its settings are, in the app. */
export const pageHref = (hc) => `/settings/plugins/${encodeURIComponent(hc.pluginId)}`;
export const settingsHref = (hc) => `${pageHref(hc)}?tab=settings`;

/** What a "needs you" button does. */
export async function act(hc, d, kind) {
	if (kind === 'alert') {
		hc.ui.navigate('/alerts');
		return { ok: true };
	}
	return hc.command(d.device_id, { action: kind });
}
