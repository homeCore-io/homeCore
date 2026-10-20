<svelte:options
	customElement={{
		tag: 'hc-sonos-speakers',
		props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } }
	}}
/>

<!-- "Sonos speakers", a card you add: every speaker, or the ones its
     settings choose — what each plays, its volume, and play or pause. -->
<script>
	import Art from './lib/Art.svelte';
	import Icon from './lib/Icon.svelte';
	import { adopt, art, hasMusic, isPlaying, nowLine, pageHref, playPause, speakersOf, volumeOf } from './lib/sonos.js';

	let { host: hc, form = 'standard', config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const all = $derived(speakersOf(devices, label));
	const shown = $derived(
		Array.isArray(config?.speakers) && config.speakers.length ? config.speakers.map((id) => all.find((d) => d.device_id === id)).filter(Boolean) : all
	);
	const title = $derived(config?.name || 'Speakers');
	const canRun = $derived(!!hc?.me.can('devices:command'));

	let error = $state('');
	async function toggle(d) {
		const r = await playPause(hc, d);
		error = r.ok ? '' : r.error;
	}
</script>

<div class="hc-tile card {form}">
	<div class="top">
		<span class="hc-tile-name">{title}</span>
		<button class="link" onclick={() => hc.ui.navigate(pageHref(hc))}>Sonos ›</button>
	</div>
	{#if shown.length}
		<div class="list">
			{#each shown as d (d.device_id)}
				<div class="row" class:off={d.available === false}>
					<button class="open" aria-label={`Open ${label(d)}`} onclick={() => hc.ui.openDevice(d.device_id)}>
						<Art src={art(d)} size={36} radius={8} label={label(d)} />
						<span class="words"><b>{label(d)}</b><span class="hc-quiet">{nowLine(d)}</span></span>
					</button>
					<span class="vol hc-num">{volumeOf(d)}</span>
					<button
						class="play"
						aria-label={`${isPlaying(d) ? 'Pause' : 'Play'} ${label(d)}`}
						disabled={!canRun || !hasMusic(d) || d.available === false}
						onclick={() => toggle(d)}><Icon name={isPlaying(d) ? 'pause' : 'play'} size={14} /></button
					>
				</div>
			{/each}
		</div>
	{:else}
		<p class="hc-quiet">No Sonos speakers found yet.</p>
	{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}
</div>

<style>
	.card {
		height: 100%;
		gap: 4px;
		padding: 16px;
		container-type: inline-size;
	}
	.top {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 10px;
		padding-bottom: 6px;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		color: var(--accent);
		font-size: 13px;
		white-space: nowrap;
	}
	.list {
		display: flex;
		flex-direction: column;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 10px;
		min-height: 52px;
		border-top: 1px solid var(--card-line);
		min-width: 0;
	}
	.row.off {
		opacity: 0.6;
	}
	.open {
		flex: 1;
		display: flex;
		align-items: center;
		gap: 10px;
		min-width: 0;
		padding: 6px 0;
		border: none;
		background: none;
		text-align: left;
	}
	.words {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}
	.words b {
		font-weight: 600;
		font-size: 14px;
	}
	.words .hc-quiet {
		font-size: 12px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.vol {
		flex: none;
		font-size: 12px;
		color: var(--ink-muted);
	}
	.play {
		flex: none;
		width: 40px;
		height: 40px;
		border-radius: 50%;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		display: flex;
		align-items: center;
		justify-content: center;
	}
	.play:disabled {
		opacity: 0.45;
		cursor: default;
	}
	.err {
		margin: 0;
		color: var(--alert);
		font-size: 13px;
	}
	@container (max-width: 260px) {
		.vol {
			display: none;
		}
		.row {
			gap: 6px;
		}
	}
</style>
