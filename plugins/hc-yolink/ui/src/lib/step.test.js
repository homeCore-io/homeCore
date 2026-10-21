import { describe, expect, it } from 'vitest';
import { build, kindOf, sentence, stateOf, stored } from './step.js';

describe('the lock step', () => {
	it('knows the four things it can say', () => {
		expect(kindOf({})).toBe('lock');
		expect(kindOf({ locked: false })).toBe('unlock');
		expect(kindOf({ auto_lock_secs: 30 })).toBe('auto');
		expect(kindOf({ sound_level: 1 })).toBe('beep');
		expect(kindOf({ sound_level: 7 })).toBe(null);
		expect(kindOf({ on: true })).toBe(null);
		expect(kindOf({ locked: true, sound_level: 1 })).toBe(null);
	});
	it('says it in words', () => {
		expect(sentence({ locked: true })).toBe('lock {devices}');
		expect(sentence({ auto_lock_secs: 30 })).toBe('{devices} locks again after 30 s');
		expect(sentence({ auto_lock_secs: 0 })).toBe('{devices} stops locking by itself');
		expect(sentence({ sound_level: 1 })).toBe('{devices} beeps quietly');
	});
	it('stores the mockup’s shapes', () => {
		expect(stored(stateOf('auto', { secs: 30 }))).toBe('{"auto_lock_secs": 30}');
		expect(stateOf('beep', { level: 1 })).toEqual({ sound_level: 1 });
		expect(build(['a'], { locked: true })).toEqual({ SetDeviceState: { device_id: 'a', state: { locked: true }, track_event_value: false } });
		expect(build(['a', 'b'], { locked: true }).Parallel.actions).toHaveLength(2);
	});
});
