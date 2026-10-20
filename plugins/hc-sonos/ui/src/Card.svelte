<svelte:options
	customElement={{
		tag: 'hc-sonos-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A speaker's card on Home and the rooms: its artwork, what it plays, the
     track's progress, play or pause and its volume. Compact, one row with a
     play button. A grouped speaker shows its group's music. -->
<script>
	import Art from './lib/Art.svelte';
	import Icon from './lib/Icon.svelte';
	import Volume from './lib/Volume.svelte';
	import { adopt, art, groupIdsOf, hasMusic, isPlaying, isRadio, nowLines, num, playPause, positionNow, volumeOf } from './lib/sonos.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(dev?.attributes ?? {});
	const name = $derived(dev ? hc.devices.label(dev) : '');
	const lines = $derived(dev ? nowLines(dev, { empty: '' }) : { title: '', sub: '' });
	const canRun = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);
	const grouped = $derived(dev ? groupIdsOf(dev).length > 1 : false);

	let seen = $state(null);
	let now = $state(Date.now());
	$effect(() => {
		const p = num(a.media_position) ?? 0;
		if (!seen || seen.pos !== p) seen = { pos: p, at: Date.now() };
	});
	$effect(() => {
		if (!isPlaying(dev)) return;
		const t = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(t);
	});
	const where = $derived(dev ? positionNow(dev, seen, now) : { pos: 0, dur: 0 });
	const showBar = $derived(hasMusic(dev) && !isRadio(dev) && where.dur > 0);

	let error = $state('');
	async function run(p) {
		const r = await p;
		error = r.ok ? '' : r.error;
	}
	const open = () => dev && hc.ui.openDevice(dev.device_id);
</script>

{#if dev}
	<div class="hc-tile {form}" class:off={!dev.available}>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		{#if form === 'compact'}
			<span class="row">
				<Art src={art(dev)} size={40} radius={8} label={lines.title} />
				<span class="words">
					<span class="hc-tile-name">{name}</span>
					<span class="hc-tile-state one">{lines.title}</span>
				</span>
				<button class="play ghost" aria-label={`${isPlaying(dev) ? 'Pause' : 'Play'} ${name}`} disabled={!canRun || !hasMusic(dev)} onclick={() => run(playPause(hc, dev))}
					><Icon name={isPlaying(dev) ? 'pause' : 'play'} size={14} /></button
				>
			</span>
		{:else}
			<span class="row">
				<Art src={art(dev)} size={form === 'large' ? 96 : 72} radius={10} label={lines.title} />
				<span class="words">
					<span class="hc-tile-name">{name}</span>
					<span class="title">{lines.title}</span>
					{#if lines.sub}<span class="hc-tile-state one">{lines.sub}</span>{/if}
					{#if grouped && form === 'large'}<span class="hc-tile-state one">Playing in a group of {groupIdsOf(dev).length}</span>{/if}
				</span>
			</span>
			{#if showBar}
				<span class="bar" role="progressbar" aria-valuemin="0" aria-valuemax={where.dur} aria-valuenow={Math.round(where.pos)}
					><span class="fill" style:width={`${(where.pos / where.dur) * 100}%`}></span></span
				>
			{/if}
			<span class="controls">
				<button class="play" aria-label={`${isPlaying(dev) ? 'Pause' : 'Play'} ${name}`} disabled={!canRun || !hasMusic(dev)} onclick={() => run(playPause(hc, dev))}
					><Icon name={isPlaying(dev) ? 'pause' : 'play'} size={16} /></button
				>
				<span class="slider"><Volume value={volumeOf(dev)} name={`${name} volume`} disabled={!canRun} onset={(v) => run(hc.command(deviceId, { volume: v }))} /></span>
			</span>
			{#if error}<span class="err" role="alert">{error}</span>{/if}
		{/if}
	</div>
{/if}

<style>
	.hc-tile {
		position: relative;
		height: 100%;
		gap: 12px;
		padding: 16px;
		container-type: inline-size;
	}
	.hc-tile.compact {
		padding: 10px 12px;
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
	.bar,
	.controls,
	.err {
		position: relative;
		z-index: 1;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
		pointer-events: none;
	}
	.compact .row {
		gap: 10px;
	}
	.row .play {
		pointer-events: auto;
	}
	.off {
		opacity: 0.6;
	}
	.words {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.title {
		font-size: 14px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.one {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.bar {
		display: block;
		height: 4px;
		border-radius: 2px;
		background: color-mix(in srgb, var(--track-off) 45%, var(--surface));
		overflow: hidden;
		pointer-events: none;
	}
	.fill {
		display: block;
		height: 100%;
		background: var(--accent);
	}
	.controls {
		display: flex;
		align-items: center;
		gap: 10px;
		margin-top: auto;
	}
	.slider {
		flex: 1;
		min-width: 0;
	}
	.play {
		flex: none;
		width: 44px;
		height: 44px;
		border-radius: 50%;
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.play.ghost {
		width: 40px;
		height: 40px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.play:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.err {
		font-size: 12px;
		color: var(--alert);
	}
	@container (max-width: 250px) {
		.standard .row :global(.art),
		.large .row :global(.art) {
			width: 52px !important;
			height: 52px !important;
		}
		.row {
			gap: 10px;
		}
	}
</style>
