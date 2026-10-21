// The automation step on a Roku, in words and as homeCore keeps it. Pure, so
// the editor's sentence and the form agree.

/** The buttons the step offers, as the remote names them. */
export const BUTTONS = [
	['VolumeUp', 'Volume up'],
	['VolumeDown', 'Volume down'],
	['VolumeMute', 'Mute'],
	['Home', 'Home'],
	['Back', 'Back'],
	['Select', 'OK'],
	['Up', 'Up'],
	['Down', 'Down'],
	['Left', 'Left'],
	['Right', 'Right'],
	['Play', 'Play / pause'],
	['Rev', 'Rewind'],
	['Fwd', 'Fast-forward'],
	['InstantReplay', 'Instant replay'],
	['Info', 'Options (✱)'],
	['ChannelUp', 'Channel up'],
	['ChannelDown', 'Channel down'],
	['FindRemote', 'Find the remote']
];
const buttonName = (k) => BUTTONS.find(([x]) => x === k)?.[1] ?? k;
/** A soundbar has no channel keys; only some Rokus can find their remote. */
export const buttonsFor = ({ tv = true, findRemote = true } = {}) =>
	BUTTONS.filter(([k]) => (tv || !k.startsWith('Channel')) && (findRemote || k !== 'FindRemote'));

const count = (v) => {
	const n = Number(v?.count ?? v?.repeat ?? 1);
	return Number.isFinite(n) ? Math.min(100, Math.max(1, Math.round(n))) : 1;
};

/** What a step does: `app`, `input`, `power`, `key` — or null for something
 * this form cannot say (left to the generic form). A new, empty step opens a
 * channel. */
export function kindOf(v) {
	if (!v || typeof v !== 'object' || !Object.keys(v).length) return 'app';
	const keys = Object.keys(v);
	if (v.action === 'launch_app' && typeof v.app === 'string') return 'app';
	if (v.action === 'select_source' && typeof v.source === 'string') return 'input';
	if (v.action === undefined && keys.length === 1 && typeof v.on === 'boolean') return 'power';
	if ((v.action === 'key' || v.action === 'keypress') && typeof v.key === 'string') return 'key';
	return null;
}

/**
 * The step as a sentence, `{devices}` where the Rokus' names go. `named`
 * turns a channel or input id into its name.
 */
export function sentence(v, named = (id) => id) {
	const k = kindOf(v);
	if (k === 'app')
		return v?.app ? `open ${named(v.app)} on {devices}` : 'open a channel on {devices}';
	if (k === 'input') return `switch {devices} to ${named(v.source)}`;
	if (k === 'power') return `turn ${v.on ? 'on' : 'off'} {devices}`;
	if (k === 'key') {
		const n = count(v);
		return `press ${buttonName(v.key)}${n > 1 ? ` ${n} times` : ''} on {devices}`;
	}
	return undefined;
}

/** The state a step sends. Channels and inputs by id: an id outlives a
 * renamed channel or input. */
export function stateOf(kind, { app, input, on = true, key = 'VolumeUp', times = 1 } = {}) {
	if (kind === 'input') return { action: 'select_source', source: input ?? '' };
	if (kind === 'power') return { on: !!on };
	if (kind === 'key') {
		const n = count({ count: times });
		return n > 1 ? { action: 'key', key, count: n } : { action: 'key', key };
	}
	return app ? { action: 'launch_app', app } : { action: 'launch_app' };
}

/** The step as homeCore keeps it: one SetDeviceState, or a Parallel of them
 * when it sets several Rokus. */
export function build(ids, state) {
	const one = (id) => ({ SetDeviceState: { device_id: id, state, track_event_value: false } });
	return ids.length === 1 ? one(ids[0]) : { Parallel: { actions: ids.map(one) } };
}

/** "Stored as", spaced as the mockup shows it. */
export const stored = (state) =>
	JSON.stringify(state).replaceAll('":', '": ').replaceAll(',"', ', "');
