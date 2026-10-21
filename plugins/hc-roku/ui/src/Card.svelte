<svelte:options
	customElement={{
		tag: 'hc-roku-card',
		props: {
			host: { type: 'Object' },
			deviceId: { type: 'String', attribute: 'device-id' },
			form: { type: 'String' }
		}
	}}
/>

<!-- A Roku's card on Home and the rooms. Playing an app: its icon, the app,
     the video's progress, play/pause, volume and power — at large, its four
     most-used channels too. On an input: "On · HDMI 1" and power. Off:
     "Off" and Turn on. Not answering: the app's offline look, no buttons. -->
<script>
	import Icon from './lib/Icon.svelte';
	import Tile from './lib/Tile.svelte';
	import {
		adopt,
		clock,
		mostUsed,
		nowOf,
		openApp,
		playPause,
		positionNow,
		stateLine
	} from './lib/roku.js';

	let { host: hc, deviceId, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const name = $derived(dev ? hc.devices.label(dev) : '');
	const now = $derived(dev ? nowOf(dev) : { kind: 'offline' });
	const canRun = $derived(!!hc?.me.can('devices:command') && dev?.available !== false);

	let tick = $state(Date.now());
	$effect(() => {
		if (now.kind !== 'app' || !now.playing) return;
		const t = setInterval(() => (tick = Date.now()), 1000);
		return () => clearInterval(t);
	});
	const where = $derived(dev && now.kind === 'app' ? positionNow(dev, tick) : null);
	const line = $derived(
		dev
			? now.kind === 'app'
				? now.paused
					? `Paused · ${now.name}`
					: now.name
				: stateLine(dev, tick)
			: ''
	);
	// At large, the four most used — other than the one already on.
	const four = $derived(
		dev && form === 'large'
			? mostUsed(dev, 5)
					.filter((x) => x.id !== now.app?.id)
					.slice(0, 4)
			: []
	);

	let error = $state('');
	async function run(p) {
		const r = await p;
		error = r.ok ? '' : r.error;
	}
	const send = (body) => run(hc.command(deviceId, body));
	const open = () => dev && hc.ui.openDevice(dev.device_id);
</script>

{#if dev}
	<div class="hc-tile {form}" class:off={!dev.available} class:dim={now.kind === 'off'}>
		<button class="cover" aria-label={`Open ${name}`} onclick={open}></button>
		<span class="row">
			<span class="pic" class:small={form === 'compact'}>
				{#if now.kind === 'app'}
					<Tile {hc} app={now.app} radius={8} />
				{:else}
					<span class="plain" class:input={now.kind === 'input'}
						>{now.kind === 'input' ? now.name : now.kind === 'home' ? 'Home' : ''}</span
					>
				{/if}
			</span>
			<span class="words">
				<span class="hc-tile-name">{name}</span>
				<span class="hc-tile-state one" class:lit={now.kind === 'app' && now.playing}>{line}</span>
			</span>
			{#if dev.available !== false && (form === 'compact' || now.kind !== 'app')}
				{#if now.kind === 'off'}
					<button class="hc-pill on-btn" disabled={!canRun} onclick={() => send({ on: true })}
						>Turn on</button
					>
				{:else if form === 'compact' && now.kind === 'app'}
					<button
						class="sq"
						aria-label={`Play or pause ${name}`}
						disabled={!canRun}
						onclick={() => run(playPause(hc, dev))}><Icon name="play-pause" size={16} /></button
					>
				{:else}
					<button
						class="sq"
						aria-label={`Turn ${name} off`}
						disabled={!canRun}
						onclick={() => send({ on: false })}><Icon name="power" size={18} /></button
					>
				{/if}
			{/if}
		</span>

		{#if form !== 'compact' && now.kind === 'app' && dev.available !== false}
			{#if where}
				<span class="progress">
					{#if where.dur}
						<span class="bar"
							><span class="fill" style:width={`${(where.pos / where.dur) * 100}%`}></span></span
						>
					{/if}
					<span class="hc-num t">{clock(where.pos)}</span>
				</span>
			{/if}
			<span class="controls">
				<button
					class="playbtn"
					aria-label={`Play or pause ${name}`}
					disabled={!canRun}
					onclick={() => run(playPause(hc, dev))}><Icon name="play-pause" size={20} /></button
				>
				<button
					class="sq"
					aria-label="Volume down"
					disabled={!canRun}
					onclick={() => send({ action: 'volume_down' })}>−</button
				>
				<button
					class="sq"
					aria-label="Volume up"
					disabled={!canRun}
					onclick={() => send({ action: 'volume_up' })}>+</button
				>
				<button
					class="sq"
					aria-label={`Turn ${name} off`}
					disabled={!canRun}
					onclick={() => send({ on: false })}><Icon name="power" size={18} /></button
				>
			</span>
		{/if}
		{#if four.length && dev.available !== false}
			<span class="four">
				{#each four as app (app.id)}
					<button
						class="chan"
						aria-label={`Open ${app.name} on ${name}`}
						disabled={!canRun}
						onclick={() => run(openApp(hc, dev, app))}><Tile {hc} {app} radius={8} /></button
					>
				{/each}
			</span>
		{/if}
		{#if error}<span class="err" role="alert">{error}</span>{/if}
	</div>
{/if}

<style>
	.hc-tile {
		position: relative;
		height: 100%;
		gap: 10px;
		padding: 14px;
		container-type: inline-size;
		box-sizing: border-box;
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
	.progress,
	.controls,
	.four,
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
	.row button {
		pointer-events: auto;
	}
	.off {
		opacity: 0.6;
	}
	.dim {
		opacity: 0.85;
	}
	.pic {
		flex: none;
		width: 56px;
	}
	.pic.small {
		width: 44px;
	}
	.plain {
		display: flex;
		align-items: center;
		justify-content: center;
		aspect-ratio: 4 / 3;
		border-radius: 8px;
		background: var(--card-line, #e9e3d6);
		font-size: 11px;
		font-weight: 600;
		text-align: center;
		box-sizing: border-box;
		padding: 2px;
	}
	.plain.input {
		background: #3d4a5c;
		color: #ffffff;
	}
	.words {
		flex: 1;
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}
	.one {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.lit {
		color: light-dark(#2f5a27, #8fd19e);
		font-weight: 600;
	}
	.progress {
		display: flex;
		align-items: center;
		gap: 8px;
		pointer-events: none;
	}
	.bar {
		flex: 1;
		height: 4px;
		border-radius: 2px;
		background: color-mix(in srgb, var(--track-off) 45%, var(--surface));
		overflow: hidden;
	}
	.fill {
		display: block;
		height: 100%;
		background: var(--accent);
	}
	.t {
		font-size: 12px;
		color: var(--ink-muted);
	}
	.controls {
		display: flex;
		gap: 8px;
		margin-top: auto;
	}
	.playbtn {
		flex: 1;
		min-height: 44px;
		border-radius: 11px;
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.sq {
		flex: none;
		min-height: 44px;
		min-width: 44px;
		border-radius: 11px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		display: flex;
		align-items: center;
		justify-content: center;
		font-size: 18px;
	}
	.compact .sq {
		min-height: 40px;
		min-width: 40px;
	}
	.on-btn {
		min-height: 44px;
		flex: none;
	}
	button:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.four {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 6px;
	}
	.chan {
		padding: 0;
		border: none;
		background: none;
		min-width: 0;
	}
	.err {
		font-size: 12px;
		color: var(--alert);
	}
</style>
