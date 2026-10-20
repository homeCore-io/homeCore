<svelte:options
	customElement={{
		tag: 'hc-sonos-favourites',
		props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } }
	}}
/>

<!-- "Sonos favourites", a card you add: the favourites with their artwork; a
     tap plays one on the speaker its settings name, or whichever is playing. -->
<script>
	import Art from './lib/Art.svelte';
	import { adopt, favouritesOf, hasMusic, isPlaying, speakersOf } from './lib/sonos.js';

	let { host: hc, form = 'standard', config = {} } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);
	const speakers = $derived(speakersOf(devices, label));
	const favs = $derived(favouritesOf(speakers));
	const canRun = $derived(!!hc?.me.can('devices:command'));

	// "playing": whichever speaker is playing now, else the first.
	const target = $derived(
		config?.speaker && config.speaker !== 'playing'
			? speakers.find((d) => d.device_id === config.speaker)
			: (speakers.find(isPlaying) ?? speakers.find(hasMusic) ?? speakers[0])
	);

	let busy = $state('');
	let error = $state('');
	async function play(f) {
		if (!target) return;
		busy = f.title;
		const r = await hc.command(target.device_id, { action: 'play_favorite', favorite: f.title });
		error = r.ok ? '' : r.error;
		setTimeout(() => (busy = ''), 800);
	}
</script>

<div class="hc-tile card {form}">
	<div class="top">
		<span class="hc-tile-name">{config?.name || 'Favourites'}</span>
		{#if target}<span class="hc-quiet on">on {label(target)}</span>{/if}
	</div>
	{#if favs.length}
		<div class="favs" class:row={form === 'compact'}>
			{#each favs as f (f.title)}
				<button class="fav" class:busy={busy === f.title} class:now={target && hasMusic(target) && target.attributes.media_title === f.title} disabled={!canRun || !target} onclick={() => play(f)}>
					{#if form === 'compact'}<Art src={f.image} size={32} radius={6} label={f.title} />{:else}<Art src={f.image} fill radius={8} label={f.title} />{/if}
					<span class="name">{f.title}</span>
				</button>
			{/each}
		</div>
	{:else}
		<p class="hc-quiet">No favourites yet — add some in the Sonos app.</p>
	{/if}
	{#if error}<p class="err" role="alert">{error}</p>{/if}
</div>

<style>
	.card {
		height: 100%;
		gap: 12px;
		padding: 16px;
	}
	.top {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 10px;
	}
	.on {
		font-size: 13px;
		white-space: nowrap;
	}
	.favs {
		display: grid;
		grid-template-columns: repeat(3, minmax(0, 1fr));
		gap: 8px;
	}
	.favs.row {
		display: flex;
		overflow-x: auto;
		scrollbar-width: thin;
	}
	.row .fav {
		flex: none;
		flex-direction: row;
		align-items: center;
		gap: 8px;
		padding: 4px 10px 4px 4px;
		max-width: 180px;
	}
	.row .fav.now {
		padding: 3px 9px 3px 3px;
	}
	.fav {
		display: flex;
		flex-direction: column;
		gap: 6px;
		padding: 6px;
		border-radius: var(--radius-chip, 11px);
		border: 1px solid var(--card-line);
		background: var(--surface);
		text-align: left;
		min-width: 0;
	}
	.fav.now {
		border: 2px solid var(--accent);
		padding: 5px;
	}
	.fav.busy {
		opacity: 0.6;
	}
	.name {
		font-size: 12px;
		font-weight: 600;
		line-height: 1.3;
	}
	.err {
		margin: 0;
		color: var(--alert);
		font-size: 13px;
	}
</style>
