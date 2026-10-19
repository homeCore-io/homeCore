<svelte:options
	customElement={{
		tag: 'hc-hue-room',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A Hue room or zone's panel: its scenes, the whole room at once, and
     each of its lights. -->
<script>
	import Wheel from './lib/Wheel.svelte';
	import Scenes from './lib/SceneTiles.svelte';
	import { adopt, FADES, fadeWords, hexToXy, kelvinToHex, num, runningIn, scenesOf, swatchOf } from './lib/hue.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const room = $derived(devices.find((d) => d.device_id === deviceId));
	const a = $derived(room?.attributes ?? {});
	const lights = $derived(
		(Array.isArray(a.light_ids) ? a.light_ids : [])
			.map((id) => devices.find((d) => d.device_id === id))
			.filter(Boolean)
	);
	const on = $derived(lights.filter((l) => l.attributes.on).length);
	const scenes = $derived(scenesOf(devices, a.group_rid));
	const running = $derived(runningIn(devices, a.group_rid));

	let dynamic = $state(false);
	let fade = $state(400);

	let error = $state('');
	async function send(id, body) {
		error = '';
		const r = await hc.command(id, body);
		if (!r.ok) error = r.error;
	}
	const all = (body) => send(deviceId, { ...body, transition_ms: fade || undefined });

	let picking = $state(false);
	const kind = $derived(a.group_kind === 'zone' ? 'Hue zone' : 'Hue room');
</script>

{#if room}
	<div class="panel">
		<!-- The app's panel above has its name, power, brightness and warmth. -->
		<span class="hc-quiet"
			>{kind} · {lights.length} light{lights.length === 1 ? '' : 's'}, {on} on{running ? ` · ${hc.devices.label(running)}` : ''}</span
		>
		{#if error}<p class="err" role="alert">{error}</p>{/if}

		<div class="cols">
			<div class="main">
				{#if scenes.length}
					<section class="hc-card">
						<div class="hc-head"><h3>Scenes</h3><span class="hc-quiet">tap to apply</span></div>
						<Scenes {hc} {scenes} columns={4} options={{ dynamic, duration_ms: fade || undefined }} onerror={(e) => (error = e)} />
						<span class="hc-row">
							<label class="check"><input type="checkbox" bind:checked={dynamic} />Keep their colours moving</label>
							<label class="check"
								>Fade in over
								<select bind:value={fade}>
									{#each FADES as ms (ms)}<option value={ms}>{fadeWords(ms)}</option>{/each}
								</select></label
							>
						</span>
					</section>
				{/if}

				<section class="hc-card">
					<h3>All its lights</h3>
					<span class="hc-row">
						<button class="hc-pill" onclick={() => all({ on: false })}>All off</button>
						{#if a.supports_color_temp}
							<button class="hc-pill" onclick={() => all({ on: true, color_temp: 2700 })}><span class="dot" style:background={kelvinToHex(2700)}></span>All warm white</button>
							<button class="hc-pill" onclick={() => all({ on: true, color_temp: 5000 })}><span class="dot" style:background={kelvinToHex(5000)}></span>All cool white</button>
						{/if}
						{#if a.supports_color_xy}
							<button class="hc-pill" class:go={picking} onclick={() => (picking = !picking)}><span class="dot rainbow"></span>All one colour…</button>
						{/if}
					</span>
					{#if picking}
						<Wheel onpick={(hex) => all({ on: true, color_xy: hexToXy(hex) })} />
					{/if}
				</section>
			</div>

			<div class="side">
				<section class="hc-card">
					<div class="hc-head"><h3>Its lights</h3><span class="hc-num">{lights.length}</span></div>
					<ul>
						{#each lights as l (l.device_id)}
							<li>
								<span class="sw" style:background={swatchOf(l.attributes)}></span>
								<button class="name" onclick={() => hc.ui.openDevice(l.device_id)}>
									<b>{hc.devices.label(l)}</b>
									<span class="hc-quiet"
										>{!l.available ? 'not answering' : l.attributes.on ? `${Math.round(num(l.attributes.brightness_pct) ?? 0)} %` : 'off'}</span
									>
								</button>
								<button
									class="tog"
									class:off={!l.attributes.on}
									role="switch"
									aria-checked={!!l.attributes.on}
									aria-label={`${hc.devices.label(l)}`}
									onclick={() => send(l.device_id, { on: !l.attributes.on })}
								></button>
							</li>
						{/each}
					</ul>
				</section>
			</div>
		</div>
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 16px;
		container-type: inline-size;
	}
	.titles {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	h2 {
		margin: 0;
		font-size: 22px;
	}
	h3 {
		margin: 0;
		font-size: 16px;
	}
	.err {
		margin: 0;
		color: var(--alert);
	}
	.cols {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 16px;
	}
	@container (min-width: 760px) {
		.cols {
			grid-template-columns: minmax(0, 1.7fr) minmax(0, 1fr);
			align-items: start;
		}
	}
	.main,
	.side {
		display: flex;
		flex-direction: column;
		gap: 16px;
		min-width: 0;
	}
	.check {
		display: inline-flex;
		align-items: center;
		gap: 8px;
		font-size: 14px;
	}
	.check select {
		min-height: 34px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		padding: 0 8px;
	}
	.track {
		width: 100%;
		appearance: none;
		height: 40px;
		border-radius: 12px;
		background: linear-gradient(90deg, #2a2620, #ffb35c);
		border: 1px solid var(--card-line);
	}
	.track::-webkit-slider-thumb {
		appearance: none;
		width: 30px;
		height: 34px;
		border-radius: 9px;
		background: #fff;
		box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35);
	}
	.track::-moz-range-thumb {
		width: 30px;
		height: 34px;
		border-radius: 9px;
		background: #fff;
		border: none;
	}
	.dot {
		width: 14px;
		height: 14px;
		border-radius: 50%;
		border: 1px solid var(--card-line);
	}
	.rainbow {
		background: conic-gradient(#f00, #ff0, #0f0, #0ff, #00f, #f0f, #f00);
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	li {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 8px 10px;
		border: 1px solid var(--card-line);
		border-radius: 12px;
	}
	.sw {
		width: 32px;
		height: 32px;
		border-radius: 9px;
		flex: none;
	}
	.name {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		border: none;
		background: none;
		padding: 0;
		text-align: left;
	}
	.name b {
		font-weight: 600;
	}
	.tog {
		width: 46px;
		height: 28px;
		border-radius: 14px;
		background: var(--track-on, var(--accent));
		position: relative;
		flex: none;
		border: none;
	}
	.tog::after {
		content: '';
		position: absolute;
		right: 3px;
		top: 3px;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: #fff;
	}
	.tog.off {
		background: var(--track-off, #cfc8ba);
	}
	.tog.off::after {
		right: auto;
		left: 3px;
	}
</style>
