import { describe, expect, it } from 'vitest';
import { build, kindOf, sentence, stateOf, stored } from './step.js';

const names = { sonos_office_1: 'Office-1', sonos_office_2: 'Office-2' };
const name = (id) => names[id] ?? id;

describe('the step', () => {
	it('says each mode the way the mockup does', () => {
		expect(sentence({ action: 'play_favorite', favorite: 'Piano for Sleeping', volume: 20 })).toBe('play Piano for Sleeping on {devices} at volume 20');
		expect(sentence({ volume: 20 })).toBe('set {devices}’s volume to 20');
		expect(sentence({ action: 'pause' })).toBe('pause {devices}');
		expect(sentence({ action: 'group', members: ['sonos_office_1', 'sonos_office_2'] }, name)).toBe('play {devices}’s music on Office-1 and Office-2 too');
		expect(sentence({ action: 'group', members: [] })).toBe('play {devices} on its own');
	});

	it('takes its four modes and a new step, and leaves the rest to the generic form', () => {
		expect(kindOf({})).toBe('fav');
		expect(kindOf({ volume: 30 })).toBe('vol');
		expect(kindOf({ action: 'group', members: 'sonos_office_2' })).toBe('group');
		expect(kindOf({ action: 'next' })).toBeNull();
		expect(kindOf({ volume: 30, muted: true })).toBeNull();
	});

	it('stores one command per speaker — the volume rides on the favourite', () => {
		expect(stateOf('fav', { favorite: 'ALT Radio', volume: 20, withVolume: true })).toEqual({ action: 'play_favorite', favorite: 'ALT Radio', volume: 20 });
		expect(stateOf('fav', { favorite: 'ALT Radio', volume: 20, withVolume: false })).toEqual({ action: 'play_favorite', favorite: 'ALT Radio' });
		expect(stateOf('group', { members: ['sonos_office_1'] })).toEqual({ action: 'group', members: ['sonos_office_1'] });
		expect(stored({ action: 'pause' })).toBe('{"action": "pause"}');
		expect(build(['sonos_bathroom'], { action: 'pause' })).toEqual({
			SetDeviceState: { device_id: 'sonos_bathroom', state: { action: 'pause' }, track_event_value: false }
		});
		expect(build(['a', 'b'], { volume: 10 }).Parallel.actions).toHaveLength(2);
	});
});
