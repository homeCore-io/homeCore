import { describe, expect, it } from 'vitest';
import { batteryLow } from './hue.js';

describe('a low battery', () => {
	const sensor = (pct) => ({ device_id: 'hue_motion_1', attributes: { battery_pct: pct } });

	it('is what homeCore’s alerts say', () => {
		const alerts = [{ device_id: 'hue_motion_1', kind: 'low_battery' }];
		expect(batteryLow(sensor(18), alerts)).toBe(true);
		// Not in Alerts (above the line, or ignored): not low here either.
		expect(batteryLow(sensor(5), [])).toBe(false);
		expect(batteryLow(sensor(5), [{ device_id: 'hue_motion_1', kind: 'offline' }])).toBe(false);
	});

	it('is homeCore’s own line on an app without alerts', () => {
		expect(batteryLow(sensor(20), undefined)).toBe(true);
		expect(batteryLow(sensor(21), undefined)).toBe(false);
		expect(batteryLow(undefined, undefined)).toBe(false);
	});
});
