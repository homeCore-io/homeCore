<svelte:options
	customElement={{
		tag: 'hc-caseta-pico-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A Pico's card on Home and the rooms: a small Pico with the last button
     pressed lit, and which it was and when. A tap opens its panel. A Pico
     takes no commands, so the card sends none. -->
<script>
	import { adopt, buttonsOf, since } from './lib/lutron.js';
	import { clock, onThePico, smallKey } from './lib/caseta.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const name = $derived(dev ? hc.devices.label(dev) : '');
	const buttons = $derived(buttonsOf(dev));
	const keys = $derived(onThePico(buttons));

	let when = $state(null);
	const stamp = $derived(`${a.last_button}:${a[`button_${a.last_button}`]}`);
	$effect(() => {
		void stamp;
		if (hc && deviceId && a.last_button !== undefined)
			void since(hc, deviceId, `button_${a.last_button}`).then((t) => (when = t));
	});
	const line = $derived(
		dev?.available === false
			? 'Offline'
			: a.last_button_name
				? `${a.last_button_name}${when ? ` · ${clock(when)}` : ''}`
				: 'No presses yet'
	);
	const open = () => dev && hc.ui.openDevice(dev.device_id);
</script>

{#if dev}
	<button
		class="hc-tile {form} card"
		class:off={dev.available === false}
		aria-label={`Open ${name}`}
		onclick={open}
	>
		{#if form !== 'compact'}
			<span class="pico" aria-hidden="true">
				{#each keys as b (b.number)}
					<span
						class="key"
						class:small={smallKey(buttons, b.number)}
						class:lit={a.last_button === b.number}
					></span>
				{/each}
			</span>
		{/if}
		<span class="words">
			<span class="hc-tile-name">{name}</span>
			<span class="hc-tile-state">{line}</span>
		</span>
	</button>
{/if}

<style>
	.card {
		height: 100%;
		width: 100%;
		flex-direction: row;
		align-items: center;
		gap: 14px;
		text-align: left;
		font: inherit;
		color: inherit;
		cursor: pointer;
	}
	.off {
		opacity: 0.6;
	}
	.pico {
		flex: none;
		width: 36px;
		box-sizing: border-box;
		padding: 8px 6px;
		border-radius: 14px;
		background: color-mix(in srgb, var(--bg, #f3f0e8) 60%, var(--surface));
		border: 1px solid var(--line);
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 4px;
	}
	.key {
		width: 22px;
		height: 8px;
		border-radius: 3px;
		background: var(--line);
	}
	.key.small {
		width: 14px;
		height: 6px;
	}
	.key.lit {
		background: color-mix(in srgb, var(--track-on) 70%, var(--surface));
	}
	.words {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
		flex: 1;
	}
</style>
