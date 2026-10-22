// The automation step on a thermostat, in words and as homeCore keeps it.
// Pure, so the editor's sentence and the form agree. Temperatures are kept in
// °C — half degrees — and said in the person's unit.

import { MODE_NAMES, num, targetText } from './thermo.js';

/** The holds' lengths offered. */
export const LENGTHS = [
	[1800, '30 minutes'],
	[3600, '1 hour'],
	[7200, '2 hours'],
	[10800, '3 hours'],
	[14400, '4 hours'],
	[28800, '8 hours'],
	[43200, '12 hours']
];

/** "2 hours", "90 minutes". */
export function lengthWords(secs) {
	const s = num(secs) ?? 0;
	const known = LENGTHS.find(([v]) => v === s);
	if (known) return known[1];
	if (s >= 3600 && s % 3600 === 0) return `${s / 3600} hours`;
	return `${Math.round(s / 60)} minutes`;
}

/** What a step does: `target`, `mode`, `hold`, `back` — or null for one
 * this form can't say (left to the generic form). A new step sets the target. */
export function kindOfStep(v) {
	if (!v || typeof v !== 'object' || !Object.keys(v).length) return 'target';
	const keys = Object.keys(v).filter((k) => !k.startsWith('_'));
	if (v.action === 'set_setpoint' && num(v.value) !== undefined) return 'target';
	if (v.action === undefined && keys.length === 1 && num(v.setpoint) !== undefined) return 'target';
	if (v.action === 'set_mode' && typeof v.value === 'string') return 'mode';
	if (v.action === undefined && keys.length === 1 && typeof v.hvac_mode === 'string') return 'mode';
	if (v.action === 'hold' && num(v.setpoint) !== undefined) return 'hold';
	if (v.action === 'resume') return 'back';
	return null;
}

/** The step's settings, whichever shape it came in. */
export function optsOf(v, fallback = {}) {
	return {
		target: num(v?.value) ?? num(v?.setpoint) ?? fallback.target ?? 21,
		mode: (typeof v?.value === 'string' ? v.value : v?.hvac_mode) ?? fallback.mode ?? 'cool',
		holdAt: num(v?.setpoint) ?? fallback.holdAt ?? fallback.target ?? 20,
		forSecs: num(v?.for_secs) ?? 7200
	};
}

/** The step as a sentence, `{devices}` where the thermostats' names go. */
export function sentence(v, units) {
	const k = kindOfStep(v);
	const o = optsOf(v);
	if (k === 'target') return `set {devices} to ${targetText(o.target, units)}`;
	if (k === 'mode') return `set {devices} to ${MODE_NAMES[o.mode] ?? o.mode}`;
	if (k === 'hold')
		return `hold {devices} at ${targetText(o.holdAt, units)} for ${lengthWords(o.forSecs)}`;
	if (k === 'back') return 'put {devices} back to its usual target';
	return undefined;
}

/** Half a degree Celsius, the step a thermostat keeps. */
export const half = (c) => Math.round(c * 2) / 2;

/** The state a step sends — °C. */
export function stateOf(kind, o) {
	if (kind === 'mode') return { action: 'set_mode', value: o.mode };
	if (kind === 'hold') return { action: 'hold', setpoint: half(o.holdAt), for_secs: o.forSecs };
	if (kind === 'back') return { action: 'resume' };
	return { action: 'set_setpoint', value: half(o.target) };
}

/** The step as homeCore keeps it: one SetDeviceState, or a Parallel of them
 * when it sets several thermostats. */
export function build(ids, state) {
	const one = (id) => ({ SetDeviceState: { device_id: id, state, track_event_value: false } });
	return ids.length === 1 ? one(ids[0]) : { Parallel: { actions: ids.map(one) } };
}

/** "Stored as", spaced as the mockup shows it. */
export const stored = (state) =>
	JSON.stringify(state).replaceAll('":', '": ').replaceAll(',"', ', "');
