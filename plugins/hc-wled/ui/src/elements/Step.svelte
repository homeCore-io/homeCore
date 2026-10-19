<svelte:options
	customElement={{
		tag: 'hc-wled-step',
		props: {
			host: { type: 'Object' },
			deviceIds: { type: 'Array', attribute: 'device-ids' },
			value: { type: 'Object' }
		},
		extend: (C) =>
			class extends C {
				/** A step this form can say: a preset, playlist, look or power on WLED. */
				static accepts(value: Record<string, unknown>): boolean {
					return stepOf(value) !== null;
				}
				/** The step in words, for the automation's sentence. */
				static describe(
					h: Host,
					ids: string[],
					value: Record<string, unknown>
				): string | undefined {
					const s = stepOf(value);
					const d = h.devices.get(ids[0] ?? '');
					if (!s || !d) return undefined;
					const dev = toCore(d);
					const name = (list: { id: number; name: string }[]) => (id: number) => nameOf(list, id);
					const said = stepSentence(s, '{devices}', {
						preset: name(presetsOf(dev)),
						effect: name(effectNames(dev)),
						palette: name(paletteNames(dev))
					});
					const out = said;
					return out.charAt(0).toLowerCase() + out.slice(1);
				}
			}
	}}
/>

<!-- The automation editor's step on WLED controllers: what to apply, on
     one controller or several at once. -->
<script lang="ts" module>
	import type { Device } from '../../../../../sdk/ui/index.js';
	import type { CoreDevice } from '#lib/devices.js';
	import { effectNames, nameOf, paletteNames, presetsOf, stepOf, stepSentence } from '#lib/wled.js';

	function toCore(d: Device): CoreDevice {
		return {
			device_id: d.device_id,
			name: d.name,
			name_override: d.name_override ?? undefined,
			plugin_id: d.plugin_id,
			device_type: d.device_type ?? undefined,
			available: d.available,
			attributes: d.attributes,
			sw_version: d.sw_version ?? undefined,
			room: d.room
		};
	}
</script>

<script lang="ts">
	import type { Host } from '#lib/host.js';
	import WledStepEdit from '#lib/components/rules/WledStepEdit.svelte';
	import { start } from './adopt.js';

	let {
		host: hc,
		deviceIds = [],
		value = {}
	}: { host: Host; deviceIds?: string[]; value?: Record<string, unknown> } = $props();
	const me = $host();
	start(me, hc);
	$effect(() => start(me, hc));

	const step = $derived(stepOf(value));
	// The whole step, as homeCore keeps it: one SetDeviceState, or a
	// Parallel of them when it sets several controllers.
	const onchange = (action: unknown) =>
		me.dispatchEvent(new CustomEvent('change', { detail: { action } }));
</script>

{#if hc && step}<WledStepEdit ids={deviceIds} {step} {onchange} />{/if}
