import { describe, expect, it } from 'vitest';
import { build, kindOf, sentence, stateOf, stored } from './step.js';

describe('the lock step', () => {
	it('knows the three things it can say', () => {
		expect(kindOf({})).toBe('lock');
		expect(kindOf({ locked: true })).toBe('lock');
		expect(kindOf({ locked: false })).toBe('unlock');
		expect(kindOf({ action: 'set_code_enabled', slot: 2, enabled: true })).toBe('code');
		expect(kindOf({ locked: true, lock_timeout_secs: 30 })).toBe(null);
		expect(kindOf({ action: 'something_else' })).toBe(null);
	});

	it('says it in words, with the code named', () => {
		const names = { 2: 'Cleaner' };
		expect(sentence({ locked: true })).toBe('lock {devices}');
		expect(sentence({ locked: false })).toBe('unlock {devices}');
		expect(sentence({ action: 'set_code_enabled', slot: 2, enabled: false }, names)).toBe('turn off code 2 · Cleaner on {devices}');
		expect(sentence({ action: 'set_code_enabled', slot: 3, enabled: true }, names)).toBe('turn on code 3 on {devices}');
	});

	it('stores the mockup’s shapes', () => {
		expect(stateOf('lock')).toEqual({ locked: true });
		expect(stateOf('unlock')).toEqual({ locked: false });
		expect(stateOf('code', { slot: 2, enabled: false })).toEqual({ action: 'set_code_enabled', slot: 2, enabled: false });
		expect(stored(stateOf('code', { slot: 2, enabled: true }))).toBe('{"action": "set_code_enabled", "slot": 2, "enabled": true}');
	});

	it('builds one step or a parallel', () => {
		expect(build(['zwave_46'], { locked: true })).toEqual({ SetDeviceState: { device_id: 'zwave_46', state: { locked: true }, track_event_value: false } });
		expect(build(['a', 'b'], { locked: true }).Parallel.actions).toHaveLength(2);
	});
});
