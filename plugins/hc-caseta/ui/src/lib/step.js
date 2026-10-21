// The automation step on a Caséta scene or Pico, in words and as homeCore
// keeps it. Pure, so the editor's sentence and the form agree.
import { num } from './lutron.js';

/** What a step does: `scene` (run it), `press` (a Pico button) — or null for
 * something this form cannot say (left to the generic form). A new, empty
 * step is `new`: the device it is on decides. */
export function kindOf(v) {
	if (!v || typeof v !== 'object' || !Object.keys(v).length) return 'new';
	if (v.action === 'activate' || v.activate === true) return 'scene';
	if (v.action === 'press') return 'press';
	return null;
}

const nameOf = (buttons, n) => buttons.find((b) => b.number === n)?.name ?? `button ${n}`;

/** The step as a sentence, `{devices}` where the devices' names go. */
export function sentence(v, buttons = [], isPico = false) {
	const k = kindOf(v);
	if (k === 'scene' || (k === 'new' && !isPico)) return 'run {devices}';
	const n = num(v?.button);
	if (k === 'press' || k === 'new')
		return n === undefined
			? 'press a button on {devices}'
			: `press ${nameOf(buttons, n)} on {devices}`;
	return undefined;
}

/** The state a step sends. */
export const stateOf = (kind, button) =>
	kind === 'press' ? { action: 'press', button } : { action: 'activate' };

/** The step as homeCore keeps it: one SetDeviceState, or a Parallel of
 * them when it sets several devices. */
export function build(ids, state) {
	const one = (id) => ({ SetDeviceState: { device_id: id, state, track_event_value: false } });
	return ids.length === 1 ? one(ids[0]) : { Parallel: { actions: ids.map(one) } };
}

/** `{"action": "press", "button": 2}`, spaced as the mockup shows it. */
export const shown = (state) => JSON.stringify(state).replaceAll(':', ': ').replaceAll(',', ', ');
