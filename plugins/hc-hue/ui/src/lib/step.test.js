import { describe, expect, it } from 'vitest';
import { build, kindOf, sentence } from './step.js';

describe('the Hue step', () => {
	it('knows what it can say, and leaves the rest', () => {
		expect(kindOf({ action: 'activate', scene_id: 'r1' })).toBe('scene');
		expect(kindOf({ on: false, transition_ms: 30000 })).toBe('power');
		expect(kindOf({ on: true, color_temp: 2700, brightness_pct: 40 })).toBe('look');
		expect(kindOf({ action: 'signal', kind: 'on_off', duration_ms: 5000 })).toBe('flash');
		expect(kindOf({ raw: { method: 'PUT' } })).toBe(null);
		expect(kindOf({ action: 'refresh' })).toBe(null);
	});

	it('says each in words', () => {
		const name = (rid) => (rid === 'r1' ? 'Savanna sunset' : undefined);
		expect(sentence({ action: 'activate', scene_id: 'r1', duration_ms: 2000 }, name)).toBe(
			'apply “Savanna sunset” in {devices}, fading over 2 s'
		);
		expect(sentence({ on: true, color_temp: 2700, brightness_pct: 40, transition_ms: 5000 })).toBe(
			'set {devices} to warm white at 40 %, fading over 5 s'
		);
		expect(sentence({ on: false, transition_ms: 30000 })).toBe('turn {devices} off, fading over 30 s');
		expect(sentence({ action: 'signal', kind: 'on_off', duration_ms: 5000 })).toBe(
			'flash {devices} for 5 s, then put them back'
		);
	});

	it('sets several devices at once as one parallel step', () => {
		expect(build(['a'], { on: true })).toEqual({
			SetDeviceState: { device_id: 'a', state: { on: true }, track_event_value: false }
		});
		expect(build(['a', 'b'], { on: true }).Parallel.actions).toHaveLength(2);
	});
});
