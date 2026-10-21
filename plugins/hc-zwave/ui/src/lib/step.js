// The automation step on a lock, in words and as homeCore keeps it. Pure, so
// the editor's sentence and the form agree.

/** What a step does: `lock`, `unlock`, `code` — or null for something this
 * form cannot say (left to the generic form). A new, empty step locks. */
export function kindOf(v) {
	if (!v || typeof v !== 'object' || !Object.keys(v).length) return 'lock';
	const keys = Object.keys(v);
	if (keys.length === 1 && v.locked === true) return 'lock';
	if (keys.length === 1 && v.locked === false) return 'unlock';
	if (v.action === 'set_code_enabled') return 'code';
	return null;
}

/** "code 2 · Cleaner", or "code 2" with no name. */
export const codeWords = (slot, names = {}) => (slot ? `code ${slot}${names[slot] ? ` · ${names[slot]}` : ''}` : 'a code');

/** The step as a sentence, `{devices}` where the lock's name goes. */
export function sentence(v, names = {}) {
	const k = kindOf(v);
	if (k === 'lock') return 'lock {devices}';
	if (k === 'unlock') return 'unlock {devices}';
	if (k === 'code') return `turn ${v.enabled === false ? 'off' : 'on'} ${codeWords(v.slot, names)} on {devices}`;
	return undefined;
}

/** The state a step sends. */
export function stateOf(kind, { slot, enabled } = {}) {
	if (kind === 'unlock') return { locked: false };
	if (kind === 'code') return { action: 'set_code_enabled', slot: slot ?? 1, enabled: enabled !== false };
	return { locked: true };
}

/** The step as homeCore keeps it: one SetDeviceState, or a Parallel of
 * them when it sets several locks. */
export function build(ids, state) {
	const one = (id) => ({ SetDeviceState: { device_id: id, state, track_event_value: false } });
	return ids.length === 1 ? one(ids[0]) : { Parallel: { actions: ids.map(one) } };
}

/** "Stored as", spaced as the mockup shows it. */
export const stored = (state) => JSON.stringify(state).replaceAll('":', '": ').replaceAll(',"', ', "');

/** Slot names learnt so far, by lock, for `describe` (which cannot wait). */
export const knownNames = new Map();
