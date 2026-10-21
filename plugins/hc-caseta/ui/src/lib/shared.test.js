// The Pico panel is hc-lutron's, copied — not a look-alike. These keep the
// copies the same as their source: change hc-lutron's, then copy it here
// (first line aside, and the panel's tag).
import { existsSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const here = (p) => new URL(p, import.meta.url);
const lutron = (p) => here(`../../../../hc-lutron/ui/src/${p}`);
const body = (url) => readFileSync(url, 'utf8').split('\n').slice(1).join('\n');
const there = existsSync(lutron('Pico.svelte'));

describe.skipIf(!there)('shared with hc-lutron', () => {
	it('has the same Pico panel', () => {
		const ours = body(here('../Pico.svelte')).replace(
			"tag: 'hc-caseta-pico'",
			"tag: 'hc-lutron-pico'"
		);
		expect(ours).toBe(readFileSync(lutron('Pico.svelte'), 'utf8'));
	});
	it('has the same helpers', () => {
		expect(body(here('./lutron.js'))).toBe(readFileSync(lutron('lib/lutron.js'), 'utf8'));
	});
});
