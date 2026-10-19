<!-- Adding a bridge: look on the network (or take an address), choose one,
     press its button, and its lights arrive. The waiting runs in the
     plugin; this asks how it is going once a second. -->
<script>
	let { hc, ondone } = $props();

	let found = $state(null);
	let looking = $state(false);
	let address = $state('');
	let status = $state({ state: 'idle' });
	let error = $state('');

	async function look() {
		looking = true;
		error = '';
		const r = await hc.action('find_bridges');
		looking = false;
		if (!r.ok) return (error = r.error);
		if (r.data.status === 'error') return (error = String(r.data.error));
		found = r.data.bridges ?? [];
	}
	$effect(() => {
		void look();
	});

	let timer;
	async function poll() {
		const r = await hc.query('pair_status');
		if (r.ok) status = r.data;
		if (['looking', 'waiting'].includes(status.state)) timer = setTimeout(poll, 1000);
	}
	$effect(() => () => clearTimeout(timer));

	async function start(params) {
		error = '';
		const r = await hc.action('pair_start', params);
		if (!r.ok) return (error = r.error);
		if (r.data.status === 'error') return (error = String(r.data.error));
		status = r.data;
		clearTimeout(timer);
		timer = setTimeout(poll, 1000);
	}
	async function cancel() {
		clearTimeout(timer);
		const r = await hc.action('pair_cancel');
		if (r.ok) status = r.data;
	}

	const step = $derived(
		status.state === 'paired' ? 4 : status.state === 'waiting' ? 3 : status.state === 'looking' ? 2 : 2
	);
	const pressed = $derived(status.state === 'waiting');
</script>

<section class="hc-card pair">
	<div class="flow">
		<h2>Add a bridge</h2>
		<ol>
			<li class:done={found !== null}><span>1</span>Look for bridges on the network</li>
			<li class:now={step === 2} class:done={step > 2}><span>2</span>Choose one</li>
			<li class:now={step === 3} class:done={step > 3}><span>3</span>Press its button</li>
			<li class:done={step === 4}><span>4</span>Its lights, rooms and scenes arrive</li>
		</ol>
		{#if error}<p class="err" role="alert">{error}</p>{/if}

		{#if status.state === 'waiting' || status.state === 'looking'}
			<p>
				{#if status.state === 'looking'}Reaching the bridge…{:else}Press the round button on top of <b>{status.name ?? 'the bridge'}</b>.{/if}
			</p>
			{#if status.state === 'waiting'}
				<div class="bar"><span style:width={`${(status.seconds_left / 90) * 100}%`}></span></div>
				<span class="hc-quiet hc-num">Waiting for the button · {status.seconds_left} s left</span>
			{/if}
			<span class="hc-row"><button class="hc-pill" onclick={cancel}>Cancel</button></span>
		{:else if status.state === 'paired'}
			<p class="ok">Added. <b>{status.name}</b> is live — its lights, rooms and scenes are arriving now.</p>
			<span class="hc-row"><button class="hc-pill go" onclick={() => ondone?.()}>Done</button></span>
		{:else}
			{#if status.state === 'failed'}<p class="err">{status.error}</p>{/if}
			{#if status.state === 'canceled'}<p class="hc-quiet">Stopped.</p>{/if}
			{#if looking}
				<p class="hc-quiet">Looking…</p>
			{:else if found}
				<span class="hc-quiet">Found {found.length}{found.some((b) => b.paired) ? ` · ${found.filter((b) => b.paired).length} already added` : ''}</span>
				{#each found as b (b.bridge_id)}
					<button class="br" disabled={b.paired} onclick={() => start({ bridge_id: b.bridge_id, name: b.name })}>
						<span><b>{b.name}</b><br /><span class="hc-quiet hc-num">{b.host} · {b.bridge_id}</span></span>
						<span class="add">{b.paired ? 'Already added' : 'Add'}</span>
					</button>
				{/each}
				<button class="hc-pill" onclick={look}>Look again</button>
			{/if}
			<details>
				<summary>Not listed? Add one by address</summary>
				<form
					class="hc-row"
					onsubmit={(e) => {
						e.preventDefault();
						if (address.trim()) void start({ host: address.trim() });
					}}
				>
					<input class="addr" bind:value={address} placeholder="10.0.10.31" aria-label="The bridge's address" />
					<button class="hc-pill go" type="submit">Add</button>
				</form>
			</details>
		{/if}
	</div>

	<svg class="bridge" viewBox="0 0 280 280" role="img" aria-label="A Hue bridge from above, its round button in the middle">
		<rect x="40" y="40" width="200" height="200" rx="44" fill="var(--surface)" stroke="var(--line)" stroke-width="2"></rect>
		<circle cx="140" cy="140" r="46" fill="var(--bg)" stroke="var(--line)" stroke-width="2"></circle>
		{#if pressed}<circle class="ring" cx="140" cy="140" r="58" fill="none" stroke="var(--track-on, var(--accent))" stroke-width="4"></circle>{/if}
		<circle cx="116" cy="210" r="5" fill="#2f8f4e"></circle>
		<circle cx="140" cy="210" r="5" fill={status.state === 'paired' ? '#2f8f4e' : 'var(--line)'}></circle>
		<circle cx="164" cy="210" r="5" fill="#2f8f4e"></circle>
	</svg>
</section>

<style>
	.pair {
		flex-direction: row;
		flex-wrap: wrap;
		gap: 20px;
	}
	.flow {
		flex: 1 1 380px;
		display: flex;
		flex-direction: column;
		gap: 12px;
		min-width: 0;
	}
	h2 {
		margin: 0;
		font-size: 19px;
	}
	ol {
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
		gap: 10px;
	}
	li span {
		width: 26px;
		height: 26px;
		border-radius: 50%;
		display: grid;
		place-items: center;
		font-size: 12px;
		font-weight: 700;
		background: var(--nav-on, var(--line));
		color: var(--ink-muted);
	}
	li.now span {
		background: var(--accent);
		color: var(--accent-ink);
	}
	li.done span {
		background: #2f8f4e;
		color: #fff;
	}
	p {
		margin: 0;
	}
	.err {
		color: var(--alert);
	}
	.ok {
		color: #2f8f4e;
	}
	.bar {
		height: 8px;
		border-radius: 4px;
		background: var(--nav-on, var(--line));
		overflow: hidden;
	}
	.bar span {
		display: block;
		height: 100%;
		background: var(--track-on, var(--accent));
		transition: width 1s linear;
	}
	.br {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 12px;
		padding: 12px 14px;
		border-radius: 12px;
		border: 1px solid var(--card-line);
		background: var(--surface);
		text-align: left;
	}
	.br:disabled {
		opacity: 0.6;
	}
	.add {
		color: var(--accent);
		font-weight: 600;
	}
	.addr {
		flex: 1;
		min-height: 38px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		padding: 0 10px;
		font-family: var(--font-num, monospace);
	}
	summary {
		cursor: pointer;
		font-size: 14px;
	}
	details form {
		margin-top: 10px;
	}
	.bridge {
		width: 220px;
		height: 220px;
		flex: none;
		align-self: center;
		margin: 0 auto;
	}
	.ring {
		animation: pulse 1.6s ease-in-out infinite;
	}
	@keyframes pulse {
		50% {
			opacity: 0.25;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.ring {
			animation: none;
		}
	}
</style>
