import { describe, expect, it } from 'vitest';
import { build, buttonsFor, kindOf, sentence, stateOf, stored } from './step.js';

const named = (id) => ({ 837: 'YouTube', 'tvinput.hdmi1': 'HDMI 1' })[id] ?? id;

describe('the Roku step', () => {
	it('says each kind of step', () => {
		expect(sentence(stateOf('app', { app: '837' }), named)).toBe('open YouTube on {devices}');
		expect(sentence(stateOf('input', { input: 'tvinput.hdmi1' }), named)).toBe(
			'switch {devices} to HDMI 1'
		);
		expect(sentence(stateOf('power', { on: true }))).toBe('turn on {devices}');
		expect(sentence(stateOf('key', { key: 'VolumeUp', times: 5 }))).toBe(
			'press Volume up 5 times on {devices}'
		);
	});
	it('takes the steps it can say and leaves the rest to the generic form', () => {
		expect(kindOf({})).toBe('app');
		expect(kindOf({ on: false })).toBe('power');
		expect(kindOf({ action: 'keypress', key: 'Home', repeat: 2 })).toBe('key');
		expect(kindOf({ action: 'tune', channel: '7.1' })).toBeNull();
		expect(kindOf({ on: true, source: 'Netflix' })).toBeNull();
	});
	it('stores what the plugin takes, as the mockup spaces it', () => {
		expect(stored(stateOf('key', { key: 'VolumeUp', times: 5 }))).toBe(
			'{"action": "key", "key": "VolumeUp", "count": 5}'
		);
		expect(build(['a', 'b'], { on: true }).Parallel.actions).toHaveLength(2);
	});
	it('offers a soundbar no channel keys', () => {
		const keys = buttonsFor({ tv: false, findRemote: true }).map(([k]) => k);
		expect(keys).not.toContain('ChannelUp');
		expect(keys).toContain('FindRemote');
		expect(buttonsFor({ tv: true, findRemote: false }).map(([k]) => k)).not.toContain('FindRemote');
	});
});
