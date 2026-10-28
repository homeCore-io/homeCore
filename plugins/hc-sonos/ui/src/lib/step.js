// The automation step on a speaker, in words and as homeCore keeps it. Pure,
// so the editor's sentence and the form agree.
import { listWords, num } from './sonos.js';

/** What a step does: `fav`, `vol`, `pause`, `group` — or null for something
 * this form cannot say (left to the generic form). A new, empty step plays a
 * favourite. */
export function kindOf(v) {
	if (!v || typeof v !== 'object' || !Object.keys(v).length) return 'fav';
	if (v.action === 'play_favorite') return 'fav';
	if (v.action === 'play_playlist') return 'list';
	if (v.action === 'pause') return 'pause';
	if (v.action === 'group') return 'group';
	if (v.action === undefined && Object.keys(v).length === 1 && num(v.volume) !== undefined) return 'vol';
	return null;
}

const quoted = (s) => s;

/** The step as a sentence, `{devices}` where the speakers' names go;
 * `names` turns device ids into names (for a group's members). */
export function sentence(v, names = (id) => id) {
	const k = kindOf(v);
	const vol = num(v?.volume);
	if (k === 'fav') {
		const what = v?.favorite ? `play ${quoted(v.favorite)}` : 'play a favourite';
		return `${what} on {devices}${vol !== undefined ? ` at volume ${vol}` : ''}`;
	}
	if (k === 'list') return `${v?.playlist ? `play the playlist ${quoted(v.playlist)}` : 'play a playlist'} on {devices}`;
	if (k === 'vol') return `set {devices}’s volume to ${vol}`;
	if (k === 'pause') return 'pause {devices}';
	if (k === 'group') {
		const m = Array.isArray(v.members) ? v.members : typeof v.members === 'string' && v.members ? [v.members] : [];
		return m.length ? `play {devices}’s music on ${listWords(m.map(names))} too` : 'play {devices} on its own';
	}
	return undefined;
}

/** The state a step sends. `volume` rides on a favourite (set on the speaker
 * before it plays) or is the whole step. */
export function stateOf(kind, { favorite, playlist, volume, withVolume, members } = {}) {
	if (kind === 'list') return playlist ? { action: 'play_playlist', playlist } : { action: 'play_playlist' };
	if (kind === 'vol') return { volume: Math.round(num(volume) ?? 20) };
	if (kind === 'pause') return { action: 'pause' };
	if (kind === 'group') return { action: 'group', members: [...(members ?? [])] };
	const s = { action: 'play_favorite' };
	if (favorite) s.favorite = favorite;
	if (withVolume) s.volume = Math.round(num(volume) ?? 20);
	return s;
}

/** The step as homeCore keeps it: one SetDeviceState, or a Parallel of
 * them when it sets several speakers. */
export function build(ids, state) {
	const one = (id) => ({ SetDeviceState: { device_id: id, state, track_event_value: false } });
	return ids.length === 1 ? one(ids[0]) : { Parallel: { actions: ids.map(one) } };
}

/** "Stored as", spaced as the mockup shows it. */
export const stored = (state) => JSON.stringify(state).replaceAll('":', '": ').replaceAll(',"', ', "');
