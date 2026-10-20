// The automation step on Hue devices, in words and as homeCore keeps it.
// Pure, so the editor's sentence and the form agree, and tests can say so.
import { effectName, fadeWords } from './hue.js';

const LOOK_KEYS = new Set(['on', 'brightness_pct', 'brightness_step', 'color_temp', 'color_xy', 'effect', 'transition_ms']);

/** What a step does: `scene`, `look`, `flash`, `pulse`, `power` — or null
 * for something this form cannot say (left to the generic form). */
export function kindOf(v) {
	if (!v || typeof v !== 'object') return null;
	if (v.action === 'activate' || v.action === 'activate_scene') return 'scene';
	if (v.action === 'signal') return 'flash';
	if (v.action === 'breathe') return 'pulse';
	if (v.action !== undefined) return null;
	const keys = Object.keys(v);
	if (!keys.length) return null;
	if (!keys.every((k) => LOOK_KEYS.has(k))) return null;
	if (keys.every((k) => k === 'on' || k === 'transition_ms')) return 'power';
	return 'look';
}

const WHITE_WORDS = [
	[2400, 'candlelight'],
	[3200, 'warm white'],
	[4500, 'neutral white'],
	[5600, 'cool white'],
	[99999, 'daylight']
];
export const whiteWord = (k) => WHITE_WORDS.find(([max]) => k <= max)[1];

const fading = (ms) => (ms ? `, fading over ${fadeWords(ms)}` : '');

/** The step as a sentence, `{devices}` where the devices' names go.
 * `sceneName(rid)` names a scene recalled through a light or room. */
export function sentence(v, sceneName = () => undefined) {
	switch (kindOf(v)) {
		case 'scene': {
			const named = v.scene_id ? sceneName(v.scene_id) : undefined;
			const what = v.scene_id ? `apply “${named ?? 'a scene'}” in {devices}` : 'apply {devices}';
			const how = [v.dynamic ? 'its colours moving' : '', v.brightness_pct ? `at ${v.brightness_pct} %` : '']
				.filter(Boolean)
				.join(', ');
			return `${what}${how ? `, ${how}` : ''}${fading(v.transition_ms)}`;
		}
		case 'flash':
			return `flash {devices}${v.kind === 'alternating' ? ' in two colours' : v.colors?.length ? ' in a colour' : ''} for ${fadeWords((v.duration_secs ?? 5) * 1000)}, then put them back`;
		case 'pulse':
			return 'pulse {devices} once';
		case 'power':
			return `turn {devices} ${v.on ? 'on' : 'off'}${fading(v.transition_ms)}`;
		case 'look': {
			const parts = [];
			if (v.effect && v.effect !== 'no_effect') parts.push(`the ${effectName(v.effect).toLowerCase()} effect`);
			else if (v.effect === 'no_effect') parts.push('no effect');
			if (v.color_temp) parts.push(whiteWord(v.color_temp));
			if (v.color_xy) parts.push('a colour');
			if (v.brightness_pct !== undefined) parts.push(`${v.brightness_pct} %`);
			if (v.brightness_step) parts.push(`${Math.abs(v.brightness_step)} % ${v.brightness_step > 0 ? 'brighter' : 'dimmer'}`);
			if (!parts.length) return `turn {devices} on${fading(v.transition_ms)}`;
			const [first, ...rest] = parts;
			const say = rest.length ? `${first} at ${rest.join(', ')}` : first;
			return `set {devices} to ${say}${fading(v.transition_ms)}`;
		}
		default:
			return undefined;
	}
}

/** The step as homeCore keeps it: one SetDeviceState, or a Parallel of
 * them when it sets several devices. */
export function build(ids, state) {
	const one = (id) => ({ SetDeviceState: { device_id: id, state, track_event_value: false } });
	return ids.length === 1 ? one(ids[0]) : { Parallel: { actions: ids.map(one) } };
}
