<svelte:options
	customElement={{
		tag: 'hc-lutron-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A keypad's card on Home and the rooms: its named buttons with their
     lights, to press, and what was pressed last. Compact, how many are lit
     and a row of lights. -->
<script>
	import { adopt, ago, buttonsOf, isRocker, ledButtonsIn, ledOf, schemaOf, since, unnamed } from './lib/lutron.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const name = $derived(dev ? hc.devices.label(dev) : '');
	const all = $derived(buttonsOf(dev).filter((b) => !isRocker(b.number)));
	const keys = $derived(all.filter((b) => !unnamed(b)));
	const left = $derived(all.length - keys.length);

	let lights = $state([]);
	$effect(() => {
		if (hc && deviceId) void schemaOf(hc, deviceId).then((s) => (lights = ledButtonsIn(s)));
	});
	const led = (n) => ledOf(dev, n) ?? (lights.includes(n) ? 0 : undefined);
	const litCount = $derived(all.filter((b) => (led(b.number) ?? 0) > 0).length);

	let when = $state(null);
	const stamp = $derived(`${a.last_button}:${a[`button_${a.last_button}`]}`);
	$effect(() => {
		void stamp;
		if (hc && deviceId && a.last_button !== undefined) void since(hc, deviceId, `button_${a.last_button}`).then((t) => (when = t));
	});
	const line = $derived(
		[dev?.room, a.last_button_name ? `last ${a.last_button_name}${when ? `, ${ago(when)}` : ''}` : 'no presses yet'].filter(Boolean).join(' · ')
	);

	const canPress = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);
	let busy = $state(0);
	async function press(n) {
		busy = n;
		await hc.command(deviceId, { action: 'press_button', button: n });
		busy = 0;
	}
	const open = () => dev && hc.ui.openDevice(dev.device_id);
</script>

{#if dev}
	<div class="hc-tile {form}" class:off={!dev.available}>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		{#if form === 'compact'}
			<span class="hc-tile-name">{name}</span>
			<span class="row">
				<span class="hc-tile-state">{litCount} lit</span>
				<span class="dots" aria-hidden="true">
					{#each all as b (b.number)}{#if led(b.number) !== undefined}<span class="led" class:on={led(b.number) > 0}></span>{/if}{/each}
				</span>
			</span>
		{:else}
			<span class="words">
				<span class="hc-tile-name">{name}</span>
				<span class="hc-tile-state">{line}</span>
			</span>
			{#if keys.length}
				<span class="keys">
					{#each keys as b (b.number)}
						{@const s = led(b.number)}
						<button class="key" disabled={!canPress || busy === b.number} aria-label={`Press ${b.name}`} onclick={() => press(b.number)}>
							{#if s !== undefined}<span class="led" class:on={s > 0}></span>{/if}<span class="kname">{b.name}</span>
						</button>
					{/each}
				</span>
			{/if}
			{#if left}<span class="hc-tile-state note">Unnamed buttons are left off the card.</span>{/if}
		{/if}
	</div>
{/if}

<style>
	.hc-tile {
		position: relative;
		height: 100%;
		gap: 12px;
		container-type: inline-size;
	}
	.cover {
		position: absolute;
		inset: 0;
		border: none;
		background: none;
		border-radius: inherit;
		z-index: 0;
	}
	.row,
	.hc-tile-name,
	.words,
	.keys,
	.note {
		position: relative;
		z-index: 1;
		pointer-events: none;
	}
	.keys {
		pointer-events: auto;
	}
	.off {
		opacity: 0.6;
	}
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
	}
	.words {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.keys {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 6px;
	}
	.key {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 44px;
		padding: 0 10px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--line);
		background: var(--sheet, var(--surface));
		text-align: left;
		font-size: 13px;
		min-width: 0;
	}
	.kname {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.led {
		flex: none;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--track-off);
	}
	.led.on {
		background: var(--track-on);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--track-on) 30%, transparent);
	}
	.dots {
		display: flex;
		gap: 5px;
		flex: none;
	}
	.compact {
		gap: 2px;
	}
	.note {
		font-size: 12px;
		margin-top: auto;
	}
	@container (max-width: 250px) {
		.keys {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
