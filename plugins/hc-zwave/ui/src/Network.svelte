<svelte:options customElement={{ tag: 'hc-zwave-network', props: { host: { type: 'Object' }, form: { type: 'String' }, config: { type: 'Object' } } }} />

<!-- "Z-Wave network", a card for a page: how many devices answer, and only
     the ones that need something — not answering, a low battery, a weak
     signal. No settings: it always shows the whole network. -->
<script>
	import { adopt, attention, pageHref, rowsOf } from './lib/zwave.js';

	let { host: hc, form = 'standard' } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const label = (d) => hc.devices.label(d);

	let status = $state(null);
	async function read() {
		const r = await hc.query('status');
		if (r.ok) status = r.data;
	}
	$effect(() => {
		if (!hc) return;
		void read();
		const t = setInterval(read, 30000);
		return () => clearInterval(t);
	});

	const rows = $derived(rowsOf(status, devices));
	const answering = $derived(rows.filter((r) => r.node.status !== 'dead').length);
	const needs = $derived(attention(status, devices, label));
	const connected = $derived(status?.connection?.state === 'connected');
</script>

<div class="hc-tile {form}">
	<div class="head">
		<span class="t">Z-Wave</span>
		<button class="link" onclick={() => hc.ui.navigate(pageHref(hc))}>Z-Wave ›</button>
	</div>
	{#if !status}
		<span class="hc-tile-state">Asking Z-Wave JS…</span>
	{:else if !connected}
		<span class="big bad">Not connected</span>
		<span class="hc-tile-state">Z-Wave JS isn’t answering; every Z-Wave device is offline.</span>
	{:else}
		<div class="count">
			<span class="n hc-num">{answering}</span><span class="of" class:bad={answering < rows.length}>of {rows.length} answering</span>
		</div>
		{#if needs.length}
			<div class="list">
				{#each needs.slice(0, form === 'large' ? 8 : 4) as x (x.id + x.what)}
					<button class="item" onclick={() => hc.ui.openDevice(x.id)}><span class="nm">{x.name}</span><span class={x.tone}>{x.what}</span></button>
				{/each}
			</div>
			<span class="foot">Only devices that need something are listed.</span>
		{:else}
			<span class="hc-tile-state">Every device is answering, with good batteries and signal.</span>
		{/if}
	{/if}
</div>

<style>
	.hc-tile {
		height: 100%;
		gap: 10px;
		padding: 16px;
	}
	.head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
	}
	.t {
		font-weight: 600;
	}
	.link {
		border: none;
		background: none;
		padding: 0;
		font-size: 13px;
		color: var(--accent);
	}
	.count {
		display: flex;
		align-items: baseline;
		gap: 8px;
	}
	.n {
		font-size: 32px;
		font-weight: 600;
		line-height: 1.1;
	}
	.of {
		font-weight: 600;
		color: light-dark(#2f5a27, #8fd19e);
	}
	.of.bad,
	.big.bad {
		color: var(--alert);
	}
	.big {
		font-size: 22px;
		font-weight: 600;
	}
	.list {
		display: flex;
		flex-direction: column;
		border-top: 1px solid var(--card-line);
	}
	.item {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
		min-height: 40px;
		border: none;
		border-bottom: 1px solid var(--card-line);
		background: none;
		color: inherit;
		font-size: 14px;
		text-align: left;
		padding: 0;
	}
	.item:last-child {
		border-bottom: none;
	}
	.nm {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.bad {
		flex: none;
		color: var(--alert);
		font-weight: 600;
	}
	.warn {
		flex: none;
		color: var(--lit-ink);
	}
	.foot {
		font-size: 12px;
		color: var(--ink-muted);
	}
</style>
