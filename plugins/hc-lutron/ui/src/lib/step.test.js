import { describe, expect, it } from 'vitest';
import { build, kindOf, sentence, stateOf } from './step.js';
import { automationsOn, faceOf, foldPresses } from './lutron.js';

const kitchen = [
	{ number: 1, name: 'OH1 Toggle' },
	{ number: 3, name: 'Outside' },
	{ number: 5, name: 'Button 5' },
	{ number: 18, name: 'Lower' },
	{ number: 19, name: 'Raise' }
];

describe('the step', () => {
	it('says presses and lights the way the mockup does', () => {
		expect(sentence({ action: 'press_button', button: 3 }, kitchen)).toBe('press Outside on {devices}');
		expect(sentence({ action: 'set_led', button: 3, state: 1 }, kitchen)).toBe('set the Outside light on {devices} on');
		// The generic form stores the state as a string.
		expect(sentence({ action: 'set_led', button: 1, state: '2' }, kitchen)).toBe('set the OH1 Toggle light on {devices} slow flash');
	});

	it('takes presses, lights and a new step, and leaves the rest alone', () => {
		expect(kindOf({})).toBe('press');
		expect(kindOf({ action: 'set_led', button: 1, state: 0 })).toBe('led');
		expect(kindOf({ press_button: 2 })).toBeNull();
		expect(sentence({ action: 'fly' })).toBeUndefined();
	});

	it('stores numbers, in the shape the plugin takes', () => {
		expect(stateOf('press', 3, 1)).toEqual({ action: 'press_button', button: 3 });
		expect(stateOf('led', 3, 2)).toEqual({ action: 'set_led', button: 3, state: 2 });
		expect(build(['lutron_73'], stateOf('press', 3))).toEqual({
			SetDeviceState: { device_id: 'lutron_73', state: { action: 'press_button', button: 3 }, track_event_value: false }
		});
		expect(build(['a', 'b'], { action: 'press_button', button: 1 }).Parallel.actions).toHaveLength(2);
	});
});

describe('a keypad', () => {
	it('draws its buttons, then the bottom rocker as lower and raise', () => {
		const f = faceOf(kitchen);
		expect(f.keys.map((b) => b.number)).toEqual([1, 3, 5]);
		expect(f.lower.number).toBe(18);
		expect(f.raise.number).toBe(19);
	});

	it('folds a press that became a hold into one "held"', () => {
		const out = foldPresses(kitchen.slice(0, 2), [
			[
				{ at: '2026-10-20T10:00:00.000Z', value: 'press' },
				{ at: '2026-10-20T10:00:00.600Z', value: 'hold' },
				{ at: '2026-10-20T10:00:02.000Z', value: 'release' }
			],
			[
				{ at: '2026-10-20T10:05:00.000Z', value: 'press' },
				{ at: '2026-10-20T10:05:00.100Z', value: 'release' }
			]
		]);
		expect(out.map((p) => `${p.name}, ${p.how}`)).toEqual(['Outside, pressed', 'OH1 Toggle, held']);
	});

	it('finds the automations a button starts', () => {
		const rules = [
			{ id: 'a', trigger: { DeviceStateChanged: { device_id: 'lutron_72', attribute: 'button_3', to: 'press' } } },
			{ id: 'b', trigger: { DeviceStateChanged: { device_id: 'x', device_ids: ['x', 'lutron_72'], attribute: 'button_3' } } },
			{ id: 'c', trigger: { DeviceStateChanged: { device_id: 'lutron_72', attribute: 'button_4' } } }
		];
		expect(automationsOn(rules, 'lutron_72', 3).map((r) => r.id)).toEqual(['a', 'b']);
	});

	it('finds the automations a button trigger starts, for that button or any', () => {
		const rules = [
			{ id: 'a', trigger: { ButtonEvent: { device_id: 'caseta_6', button_number: 2, event: 'Pushed' } } },
			{ id: 'b', trigger: { ButtonEvent: { device_id: 'caseta_6', event: 'Held' } } },
			{ id: 'c', trigger: { ButtonEvent: { device_id: 'caseta_6', button_number: 4, event: 'Pushed' } } },
			{ id: 'd', trigger: { ButtonEvent: { device_id: 'caseta_9', button_number: 2, event: 'Pushed' } } }
		];
		expect(automationsOn(rules, 'caseta_6', 2).map((r) => r.id)).toEqual(['a', 'b']);
	});
});
