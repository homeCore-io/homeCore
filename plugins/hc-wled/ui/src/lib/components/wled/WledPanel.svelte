<!--
	A WLED controller, all of it: its look (effect, palette, colours — the
	controls the effect uses, by the names the controller gives them), its
	segments, presets and playlists, and the controller itself (nightlight,
	sync, live data, sound, firmware, setup). Shown in the device's details in
	place of the generated controls; the card's power and brightness stay above.
-->
<script lang="ts">
	import { page } from '$app/state';
	import type { CoreDevice } from '#lib/devices.js';
	import { devices } from '#lib/devices.svelte.js';
	import { openDevice } from '#lib/popup.js';
	import { wledCatalogs } from '#lib/wledCatalog.svelte.js';
	import {
		isWled,
		mainSegment,
		nameOf,
		effectNames,
		presetsOf,
		segmentCss,
		segmentsOf
	} from '#lib/wled.js';
	import WledLook from './WledLook.svelte';
	import WledSegments from './WledSegments.svelte';
	import WledPresets from './WledPresets.svelte';
	import WledPlaylists from './WledPlaylists.svelte';
	import WledController from './WledController.svelte';

	let { device, disabled }: { device: CoreDevice; disabled: boolean } = $props();

	const TABS = ['Look', 'Segments', 'Presets', 'Playlists', 'Controller'] as const;
	type Tab = (typeof TABS)[number];
	let tab = $state<Tab>('Look');

	$effect(() => wledCatalogs.ensure(device));
	const entry = $derived(wledCatalogs.of(device));
	const catalog = $derived(entry.catalog);

	const others = $derived(
		Object.values(devices.byId)
			.filter((d) => isWled(d))
			.sort((a, b) => (a.name_override ?? a.name).localeCompare(b.name_override ?? b.name))
	);
	const seg = $derived(mainSegment(device));
	const presets = $derived(presetsOf(device));
	const running = $derived(presets.find((p) => p.id === device.attributes.preset_id));
	const playlist = $derived(presets.find((p) => p.id === device.attributes.playlist_id));
	const fx = $derived(nameOf(effectNames(device), seg?.effect_id));
	const counts: Partial<Record<Tab, number>> = $derived({
		Segments: segmentsOf(device).length,
		Presets: presets.filter((p) => p.kind !== 'playlist').length,
		Playlists: presets.filter((p) => p.kind === 'playlist').length
	});
	const label = (d: CoreDevice) => d.name_override ?? d.name;
</script>

<div class="wled">
	{#if others.length > 1}
		<label class="switch">
			<span>Controller</span>
			<select
				value={device.device_id}
				onchange={(e) => openDevice(e.currentTarget.value, page.url)}
			>
				{#each others as o (o.device_id)}
					<option value={o.device_id}>{label(o)}{o.available ? '' : ' — offline'}</option>
				{/each}
			</select>
		</label>
	{/if}

	<div class="now">
		<div class="strip" style:background={segmentCss(seg, catalog)} aria-hidden="true"></div>
		<p>
			{#if playlist}Playlist “{playlist.name}”{#if running}, now {running.name}{/if}
			{:else if running}Preset “{running.name}”{#if fx}
					· {fx}{/if}
			{:else if fx}Effect “{fx}”{:else}…{/if}
			{#if device.attributes.live === true}<span class="tag"
					>live data{device.attributes.live_source
						? ` from ${device.attributes.live_source}`
						: ''}</span
				>{/if}
		</p>
	</div>

	<div class="tabs" role="tablist">
		{#each TABS as t (t)}
			<button role="tab" aria-selected={tab === t} onclick={() => (tab = t)}
				>{t}{#if counts[t] !== undefined}<span class="num">{counts[t]}</span>{/if}</button
			>
		{/each}
	</div>

	{#if entry.error && !catalog}
		<p class="warn">
			The effect catalogue didn't load: {entry.error}
			<button class="link" onclick={() => wledCatalogs.load(device, entry.firmware)}
				>Try again</button
			>
		</p>
	{/if}

	{#if tab === 'Look'}
		<WledLook {device} {catalog} {disabled} />
	{:else if tab === 'Segments'}
		<WledSegments {device} {catalog} {disabled} />
	{:else if tab === 'Presets'}
		<WledPresets {device} {catalog} {disabled} />
	{:else if tab === 'Playlists'}
		<WledPlaylists {device} {disabled} />
	{:else}
		<WledController {device} {disabled} />
	{/if}
</div>

<style>
	.wled {
		display: flex;
		flex-direction: column;
		gap: 14px;
		min-width: 0;
	}
	.switch {
		display: flex;
		gap: 8px;
		align-items: center;
		font-size: 14px;
		color: var(--ink-muted);
	}
	.switch select {
		flex: 1;
		min-width: 0;
		font: inherit;
		color: var(--ink);
		min-height: 40px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		padding: 0 10px;
	}
	.now {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.strip {
		height: 18px;
		border-radius: 6px;
		border: 1px solid var(--card-line);
	}
	.now p {
		margin: 0;
		font-size: 14px;
		color: var(--ink-muted);
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: center;
	}
	.tag {
		font-size: 12px;
		font-weight: 600;
		color: var(--lit-ink);
		background: var(--lit-bg);
		border-radius: 6px;
		padding: 1px 6px;
	}
	.tabs {
		display: flex;
		gap: 2px;
		border-bottom: 1px solid var(--line);
		overflow-x: auto;
	}
	.tabs button {
		font: inherit;
		font-size: 15px;
		min-height: var(--target);
		padding: 0 12px;
		border: none;
		background: none;
		color: var(--ink-muted);
		border-bottom: 3px solid transparent;
		white-space: nowrap;
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.tabs button[aria-selected='true'] {
		color: var(--ink);
		font-weight: 700;
		border-bottom-color: var(--accent);
	}
	.num {
		font-family: var(--font-num);
		font-size: 12px;
		color: var(--ink-muted);
	}
	.warn {
		margin: 0;
		font-size: 14px;
		color: var(--alert);
	}
	.link {
		font: inherit;
		border: none;
		background: none;
		color: var(--accent);
		text-decoration: underline;
		padding: 0;
	}
</style>
