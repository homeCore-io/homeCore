import { describe, expect, it } from 'vitest';
import { build, cardWords, rulesFor, tellsWords } from './tell.js';

const C = { units: { show: (v, u) => ({ value: v, unit: u, text: `${v} ${u}` }) } };

describe('tell me when', () => {
	it('finds a sensor’s own temperature rules', () => {
		const rules = [
			{ id: 'a', device_ids: ['f'], attribute: 'temperature', op: 'above', value: -10, enabled: true },
			{ id: 'b', device_ids: ['f', 'g'], attribute: 'temperature', op: 'above', value: 0 },
			{ id: 'c', device_ids: ['f'], attribute: 'open', op: 'eq', value: true }
		];
		const r = rulesFor(rules, 'f');
		expect(r.above.id).toBe('a');
		expect(r.below).toBeUndefined();
		expect(tellsWords(C, r)).toBe('warmer than −10 °C');
		expect(cardWords(C, r)).toBe('Tells you above −10 °C');
	});
	it('builds a core alert rule in °C, keeping what it doesn’t set', () => {
		const r = build({ deviceId: 'f', deviceName: 'Garage Freezer', op: 'above', value: -10, forMinutes: 15, notify: '' });
		expect(r).toEqual({ name: 'Garage Freezer warming up', enabled: true, device_ids: ['f'], attribute: 'temperature', op: 'above', value: -10, for_minutes: 15, state_label: 'Warmer than -10°C', severity: 'warning' });
		const again = build({ was: { ...r, id: 'x', after: '22:00' }, deviceId: 'f', deviceName: 'Garage Freezer', op: 'above', value: -12, forMinutes: 0, notify: 'Phone', enabled: false });
		expect(again.id).toBe('x');
		expect(again.after).toBe('22:00');
		expect(again.for_minutes).toBeUndefined();
		expect(again.notify).toBe('Phone');
		expect(again.enabled).toBe(false);
	});
});
