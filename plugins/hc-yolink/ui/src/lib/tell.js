// "Tell me when" — a temperature sensor's limits, kept as homeCore's own
// alert rules (POST/PUT /alert-rules), not in the plugin: they show on the
// bell and the Alerts page, clear by themselves, can start automations ("Do
// something…", Settings › Alert rules) and keep working whatever plugin the
// sensor comes from. Values are °C, as homeCore keeps temperatures.
//
// Reading rules needs `automations:read`; changing them `automations:write`
// — the same as the app's own alert-rules page.

import { temp } from './yolink.js';

/** How long it must last: minutes; 0 is "at once". */
export const FOR_CHOICES = [
	[0, 'at once'],
	[5, '5 minutes'],
	[15, '15 minutes'],
	[30, '30 minutes'],
	[60, '1 hour']
];

const forThis = (r, id) => Array.isArray(r?.device_ids) && r.device_ids.length === 1 && r.device_ids[0] === id && r.attribute === 'temperature';

/** A device's two temperature rules: `{ above, below }` (undefined where none). */
export function rulesFor(rules, id) {
	const mine = (rules ?? []).filter((r) => forThis(r, id));
	const pick = (op) => mine.find((r) => r.op === op && r.enabled !== false) ?? mine.find((r) => r.op === op);
	return { above: pick('above'), below: pick('below') };
}

/** The rule's name: "Garage Freezer warming up", "… too cold". */
export function ruleName(device, op) {
	return `${device} ${op === 'above' ? 'warming up' : 'too cold'}`;
}

/**
 * The rule as homeCore keeps it. `value` °C; `forMinutes` 0 for at once;
 * `notify` '' for nobody (shown, not sent), 'all', or a channel's name.
 * Keeps the fields of `was` it doesn't set (its id, its times of day).
 */
export function build({ was, deviceId, deviceName, op, value, forMinutes = 0, notify = '', enabled = true, label }) {
	const r = {
		...(was ?? {}),
		name: was?.name ?? ruleName(deviceName, op),
		enabled,
		device_ids: [deviceId],
		attribute: 'temperature',
		op,
		value,
		state_label: label ?? `${op === 'above' ? 'Warmer' : 'Colder'} than ${value}°C`,
		severity: was?.severity ?? 'warning'
	};
	if (forMinutes > 0) r.for_minutes = forMinutes;
	else delete r.for_minutes;
	if (notify) r.notify = notify;
	else delete r.notify;
	return r;
}

/** "warmer than −10 °C" (in the person's units), or '' for none. */
export function tellsWords(hc, { above, below } = {}) {
	const parts = [];
	if (above && above.enabled !== false) parts.push(`warmer than ${temp(hc, above.value)}`);
	if (below && below.enabled !== false) parts.push(`colder than ${temp(hc, below.value)}`);
	return parts.join(' or ');
}

/** A card's line: "Tells you above −10 °C". */
export function cardWords(hc, { above, below } = {}) {
	const on = (r) => r && r.enabled !== false;
	if (on(above) && on(below)) return `Tells you outside ${temp(hc, below.value)} to ${temp(hc, above.value)}`;
	if (on(above)) return `Tells you above ${temp(hc, above.value)}`;
	if (on(below)) return `Tells you below ${temp(hc, below.value)}`;
	return '';
}

/** The house's alert rules, or why not: `{ rules, error }`. */
export async function readRules(hc) {
	try {
		const r = await hc.fetch('/alert-rules');
		if (r.status === 401 || r.status === 403) return { rules: [], error: 'forbidden' };
		if (!r.ok) return { rules: [], error: `homeCore said ${r.status}` };
		const list = await r.json();
		return { rules: Array.isArray(list) ? list : [], error: '' };
	} catch {
		return { rules: [], error: 'homeCore is not answering' };
	}
}

/** Save a rule: PUT if it has an id, else POST. The saved rule, or an error. */
export async function saveRule(hc, rule) {
	const path = rule.id ? `/alert-rules/${encodeURIComponent(rule.id)}` : '/alert-rules';
	try {
		const r = await hc.fetch(path, {
			method: rule.id ? 'PUT' : 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(rule)
		});
		const body = await r.json().catch(() => ({}));
		if (!r.ok) return { error: body.error ?? `homeCore said ${r.status}` };
		return { rule: body };
	} catch {
		return { error: 'homeCore is not answering' };
	}
}

/** Where a rule's alerts can be told: `['all', …channel names]`, as far as
 * this person can read them (the channels are in the house's settings). */
export async function channels(hc) {
	try {
		const r = await hc.fetch('/system/config');
		if (!r.ok) return [];
		const c = await r.json();
		const list = c?.parsed?.notify?.channels;
		return Array.isArray(list) ? list.map((x) => x?.name).filter(Boolean) : [];
	} catch {
		return [];
	}
}
