import { describe, expect, it } from 'vitest';
import { build, kindOfStep, lengthWords, optsOf, sentence, stateOf, stored } from './step.js';

const F = { temperature: 'F' };

describe('the automation step', () => {
	it('knows the four kinds, and leaves anything else to the generic form', () => {
		expect(kindOfStep({})).toBe('target');
		expect(kindOfStep({ action: 'set_setpoint', value: 20 })).toBe('target');
		expect(kindOfStep({ setpoint: 20 })).toBe('target');
		expect(kindOfStep({ action: 'set_mode', value: 'cool' })).toBe('mode');
		expect(kindOfStep({ hvac_mode: 'off' })).toBe('mode');
		expect(kindOfStep({ action: 'hold', setpoint: 19, for_secs: 7200 })).toBe('hold');
		expect(kindOfStep({ action: 'resume' })).toBe('back');
		expect(kindOfStep({ action: 'set_hysteresis', value: 1 })).toBeNull();
		expect(kindOfStep({ setpoint: 20, hvac_mode: 'cool' })).toBeNull();
	});

	it('is kept in °C, half degrees', () => {
		expect(stateOf('target', { target: 21.11 })).toEqual({ action: 'set_setpoint', value: 21 });
		expect(stateOf('mode', { mode: 'cool' })).toEqual({ action: 'set_mode', value: 'cool' });
		expect(stateOf('hold', { holdAt: 18.9, forSecs: 7200 })).toEqual({
			action: 'hold',
			setpoint: 19,
			for_secs: 7200
		});
		expect(stateOf('back', {})).toEqual({ action: 'resume' });
	});

	it('is said in the person’s unit', () => {
		expect(sentence({ action: 'set_setpoint', value: 21 }, F)).toBe('set {devices} to 70°');
		expect(sentence({ action: 'set_mode', value: 'cool' }, F)).toBe('set {devices} to Cool');
		expect(sentence({ action: 'hold', setpoint: 19, for_secs: 7200 }, F)).toBe(
			'hold {devices} at 66° for 2 hours'
		);
		expect(sentence({ action: 'resume' })).toBe('put {devices} back to its usual target');
		expect(sentence({ action: 'set_setpoint', value: 21 })).toBe('set {devices} to 21°');
		expect(lengthWords(5400)).toBe('90 minutes');
	});

	it('reads back what it stored', () => {
		expect(optsOf({ action: 'hold', setpoint: 19, for_secs: 3600 })).toMatchObject({
			holdAt: 19,
			forSecs: 3600
		});
		expect(stored({ action: 'hold', setpoint: 19, for_secs: 7200 })).toBe(
			'{"action": "hold", "setpoint": 19, "for_secs": 7200}'
		);
	});

	it('sets several at once as one Parallel', () => {
		const s = { action: 'resume' };
		expect(build(['a'], s)).toEqual({
			SetDeviceState: { device_id: 'a', state: s, track_event_value: false }
		});
		expect(build(['a', 'b'], s).Parallel.actions).toHaveLength(2);
	});
});
