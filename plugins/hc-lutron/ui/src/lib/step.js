// The automation step on a keypad or visor receiver, in words and as
// homeCore keeps it. Pure, so the editor's sentence and the form agree.
import { LED_STATES, num } from './lutron.js';

/** What a step does: `press`, `led` — or null for something this form
 * cannot say (left to the generic form). A new, empty step is a press. */
export function kindOf(v) {
	if (!v || typeof v !== 'object' || !Object.keys(v).length) return 'press';
	if (v.action === 'press_button') return 'press';
	if (v.action === 'set_led') return 'led';
	return null;
}

const nameOf = (buttons, n) => buttons.find((b) => b.number === n)?.name ?? `button ${n}`;

/** The step as a sentence, `{devices}` where the devices' names go. */
export function sentence(v, buttons = []) {
	const k = kindOf(v);
	const n = num(v?.button);
	if (k === 'press') return n === undefined ? 'press a button on {devices}' : `press ${nameOf(buttons, n)} on {devices}`;
	if (k === 'led') {
		const word = LED_STATES.find((s) => s.value === num(v.state))?.word ?? 'on';
		return n === undefined ? `set a light on {devices} ${word}` : `set the ${nameOf(buttons, n)} light on {devices} ${word}`;
	}
	return undefined;
}

/** The state a step sends: numbers, as the plugin's actions take them. */
export function stateOf(kind, button, led) {
	return kind === 'led' ? { action: 'set_led', button, state: led } : { action: 'press_button', button };
}

/** The step as homeCore keeps it: one SetDeviceState, or a Parallel of
 * them when it sets several devices. */
export function build(ids, state) {
	const one = (id) => ({ SetDeviceState: { device_id: id, state, track_event_value: false } });
	return ids.length === 1 ? one(ids[0]) : { Parallel: { actions: ids.map(one) } };
}
