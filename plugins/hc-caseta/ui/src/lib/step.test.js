import { describe, expect, it } from 'vitest';
import { build, kindOf, sentence, shown, stateOf } from './step.js';
import { byButton, nowWords, onThePico, usedByWords, usesOf, worksAs } from './caseta.js';
import { automationsOn } from './lutron.js';

const pico = [
	{ number: 2, name: 'On' },
	{ number: 3, name: 'Favorite' },
	{ number: 4, name: 'Off' },
	{ number: 5, name: 'Raise' },
	{ number: 6, name: 'Lower' }
];

describe('the step', () => {
	it('says scenes and presses the way the mockup does', () => {
		expect(sentence({ action: 'activate' })).toBe('run {devices}');
		expect(sentence({ action: 'press', button: 5 }, pico, true)).toBe('press Raise on {devices}');
		// The generic form stores the button as a string.
		expect(sentence({ action: 'press', button: '2' }, pico, true)).toBe('press On on {devices}');
		expect(sentence({}, pico, true)).toBe('press a button on {devices}');
		expect(sentence({}, [], false)).toBe('run {devices}');
	});

	it('takes scenes, presses and a new step, and leaves the rest alone', () => {
		expect(kindOf({})).toBe('new');
		expect(kindOf({ action: 'activate' })).toBe('scene');
		expect(kindOf({ activate: true })).toBe('scene');
		expect(kindOf({ action: 'press', button: 2 })).toBe('press');
		expect(kindOf({ on: true })).toBeNull();
		expect(sentence({ on: true })).toBeUndefined();
	});

	it('stores what the plugin takes', () => {
		expect(stateOf('scene')).toEqual({ action: 'activate' });
		expect(stateOf('press', 4)).toEqual({ action: 'press', button: 4 });
		expect(build(['caseta_6'], stateOf('press', 2))).toEqual({
			SetDeviceState: {
				device_id: 'caseta_6',
				state: { action: 'press', button: 2 },
				track_event_value: false
			}
		});
		expect(build(['a', 'b'], stateOf('scene')).Parallel.actions).toHaveLength(2);
		expect(shown(stateOf('press', 2))).toBe('{"action": "press", "button": 2}');
	});
});

describe('the page', () => {
	it('says what each kind works as, and what a light is doing', () => {
		expect(worksAs('switch')).toBe('On/off');
		expect(worksAs('dimmer')).toBe('Dimmer');
		expect(nowWords({ available: true, attributes: { on: true } })).toBe('On');
		expect(nowWords({ available: true, attributes: { on: true, brightness_pct: 40 } })).toBe(
			'On · 40 %'
		);
		expect(nowWords({ available: false, attributes: { on: true } })).toBe('Offline');
	});

	it('counts the automations that run a scene', () => {
		const rules = [
			{
				id: 'a',
				actions: [
					{ action: { SetDeviceState: { device_id: 'caseta_scene_sonos_play', state: {} } } }
				]
			},
			{
				id: 'b',
				actions: [
					{
						action: {
							Conditional: {
								then_actions: [
									{ SetDeviceState: { device_id: 'caseta_scene_sonos_play', state: {} } }
								]
							}
						}
					}
				]
			},
			{
				id: 'c',
				trigger: { DeviceStateChanged: { device_id: 'caseta_scene_sonos_play' } },
				actions: []
			}
		];
		expect(usesOf(rules, 'caseta_scene_sonos_play').map((r) => r.id)).toEqual(['a', 'b']);
		expect(usedByWords(0)).toBe('Not used by an automation');
		expect(usedByWords(1)).toBe('Used by 1 automation');
		expect(usedByWords(3)).toBe('Used by 3 automations');
	});

	it('lists scenes by phantom button', () => {
		const s = [
			{ device_id: 'b', name: 'B' },
			{ device_id: 'a', name: 'A' }
		];
		const st = {
			scenes: [
				{ device_id: 'a', button: 2 },
				{ device_id: 'b', button: 1 }
			]
		};
		expect(byButton(s, st).map((x) => x.device_id)).toEqual(['b', 'a']);
	});
});

describe('a Pico', () => {
	it('is drawn top to bottom as it is held: On, Raise, Favorite, Lower, Off', () => {
		expect(onThePico(pico).map((b) => b.name)).toEqual(['On', 'Raise', 'Favorite', 'Lower', 'Off']);
		const scene = [8, 9, 10, 11].map((number) => ({ number, name: `${number}` }));
		expect(onThePico(scene).map((b) => b.number)).toEqual([8, 9, 10, 11]);
	});

	it("knows a Caséta Pico's button triggers", () => {
		const rules = [
			{
				id: 'a',
				trigger: { ButtonEvent: { device_id: 'caseta_6', button_number: 2, event: 'Pushed' } }
			},
			{
				id: 'b',
				trigger: {
					DeviceStateChanged: { device_id: 'caseta_6', attribute: 'button_2', to: 'press' }
				}
			}
		];
		expect(automationsOn(rules, 'caseta_6', 2).map((r) => r.id)).toEqual(['a', 'b']);
		expect(automationsOn(rules, 'caseta_6', 4)).toEqual([]);
	});
});
