// The automation step on a YoLink lock, in words and as homeCore keeps it.
// Pure, so the editor's sentence and the form agree.

import { secsWords } from './yolink.js';

/** What a step does: `lock`, `unlock`, `auto`, `beep` — or null for
 * something this form cannot say (left to the generic form). A new, empty
 * step locks. */
export function kindOf(v) {
	if (!v || typeof v !== 'object' || !Object.keys(v).length) return 'lock';
	const keys = Object.keys(v);
	if (keys.length !== 1) return null;
	if (v.locked === true) return 'lock';
	if (v.locked === false) return 'unlock';
	if (Number.isInteger(v.auto_lock_secs) && v.auto_lock_secs >= 0) return 'auto';
	if (Number.isInteger(v.sound_level) && v.sound_level >= 0 && v.sound_level <= 3) return 'beep';
	return null;
}

const BEEP = ['beeps not at all', 'beeps quietly', 'beeps at medium volume', 'beeps loudly'];

/** The step as a sentence, `{devices}` where the lock's name goes. */
export function sentence(v) {
	const k = kindOf(v);
	if (k === 'lock') return 'lock {devices}';
	if (k === 'unlock') return 'unlock {devices}';
	if (k === 'auto') return v.auto_lock_secs > 0 ? `{devices} locks again after ${secsWords(v.auto_lock_secs)}` : '{devices} stops locking by itself';
	if (k === 'beep') return `{devices} ${BEEP[v.sound_level]}`;
	return undefined;
}

/** The state a step sends. */
export function stateOf(kind, { secs = 30, level = 1 } = {}) {
	if (kind === 'unlock') return { locked: false };
	if (kind === 'auto') return { auto_lock_secs: secs };
	if (kind === 'beep') return { sound_level: level };
	return { locked: true };
}

/** One SetDeviceState, or a Parallel of them for several locks. */
export function build(ids, state) {
	const one = (id) => ({ SetDeviceState: { device_id: id, state, track_event_value: false } });
	return ids.length === 1 ? one(ids[0]) : { Parallel: { actions: ids.map(one) } };
}

/** "Stored as", spaced as the mockup shows it. */
export const stored = (state) => JSON.stringify(state).replaceAll('":', '": ').replaceAll(',"', ', "');
