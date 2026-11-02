<svelte:options customElement={{ tag: 'hc-nuheat-page', props: { host: { type: 'Object' } } }} />

<!-- hc-nuheat's page: whether it is signed in to NuHeat, as whom and until
     when, the thermostats it found, and the way to sign in. Signing in goes
     to NuHeat's own site; NuHeat sends the browser to homecore.io, which sends
     it back here with a code, and this page hands that to the plugin. Its
     settings are the app's generic form, below this page. -->
<script>
	/** @type {{ host: import('../../../../sdk/ui/index.js').Host }} */
	let { host: hc } = $props();
	const me = $host();

	// The app's parts (hc-card, hc-pill, …) in this shadow root.
	$effect(() => {
		const root = me?.shadowRoot;
		const sheet = hc?.theme?.sheet;
		if (root && sheet && !root.adoptedStyleSheets.includes(sheet))
			root.adoptedStyleSheets = [sheet, ...root.adoptedStyleSheets];
	});

	let status = $state(null);
	let trouble = $state('');
	async function load() {
		const r = await hc.query('status', {});
		if (r.ok) {
			status = r.data.result ?? r.data;
			trouble = '';
		} else trouble = r.error;
	}
	$effect(() => {
		if (!hc) return;
		void load();
		const t = setInterval(load, 30_000);
		return () => clearInterval(t);
	});

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	let alerts = $state([]);
	$effect(() => hc?.alerts?.subscribe((list) => (alerts = list ?? [])));

	const admin = $derived(hc?.me?.isAdmin ?? false);

	// ---- coming back from NuHeat (through homecore.io) ----
	let busy = $state('');
	let said = $state('');
	let failed = $state('');
	let finished = false;
	$effect(() => {
		if (!hc || finished) return;
		const q = new URLSearchParams(location.search);
		if (!q.has('state') || !(q.has('code') || q.has('error'))) return;
		finished = true;
		void finish({
			code: q.get('code') ?? undefined,
			state: q.get('state') ?? undefined,
			error: q.get('error') ?? undefined
		});
		// The code is spent either way; it shouldn't stay in the address.
		const clean = new URL(location.href);
		for (const k of ['code', 'state', 'error', 'error_description', 'scope', 'session_state', 'iss'])
			clean.searchParams.delete(k);
		hc.ui.navigate(`${clean.pathname}${clean.search}`);
	});

	async function finish(params) {
		busy = 'Checking your sign-in with NuHeat…';
		failed = '';
		said = '';
		const r = await hc.action('complete_link', params);
		busy = '';
		if (r.ok && r.data.status !== 'error') {
			said = r.data.account ? `Linked to ${r.data.account}.` : 'Linked.';
			pasting = false;
			pasted = '';
		} else failed = r.ok ? r.data.error : r.error;
		void load();
	}

	// ---- going to NuHeat ----
	async function link() {
		busy = 'Opening NuHeat…';
		failed = '';
		const back = `${location.origin}${location.pathname}`;
		const r = await hc.action('begin_link', { via: 'relay', return_to: back });
		if (!r.ok || r.data.status === 'error' || !r.data.url) {
			busy = '';
			failed = r.ok ? (r.data.error ?? 'The plugin gave no address.') : r.error;
			return;
		}
		location.assign(r.data.url);
	}

	// The fallback: sign in in another tab, paste the address back.
	let pasting = $state(false);
	let pasted = $state('');
	async function linkByPasting() {
		failed = '';
		const r = await hc.action('begin_link', { via: 'loopback' });
		if (!r.ok || r.data.status === 'error' || !r.data.url) {
			failed = r.ok ? (r.data.error ?? 'The plugin gave no address.') : r.error;
			return;
		}
		pasting = true;
		window.open(r.data.url, '_blank', 'noopener');
	}

	async function signOut() {
		const go = await hc.ui.confirm({
			title: 'Sign out of NuHeat?',
			body: 'The thermostats stay in homeCore, without readings or control, until you link again.',
			go: 'Sign out',
			danger: true
		});
		if (!go) return;
		await hc.action('sign_out', {});
		said = 'Signed out.';
		void load();
	}

	let refreshing = $state(false);
	async function refresh() {
		refreshing = true;
		await hc.action('refresh', {});
		setTimeout(() => {
			refreshing = false;
			void load();
		}, 3000);
	}

	// ---- saying when ----
	let now = $state(Date.now());
	$effect(() => {
		const t = setInterval(() => (now = Date.now()), 60_000);
		return () => clearInterval(t);
	});
	const day = (iso) =>
		new Date(iso).toLocaleDateString(undefined, { month: 'long', day: 'numeric' });

	const deviceOf = (id) => devices.find((d) => d.device_id === id);
	const reading = (d) => {
		const a = d?.attributes ?? {};
		if (typeof a.temperature !== 'number') return '—';
		const show = (v) => (hc?.units ? hc.units.show(v, '°C').text : `${Math.round(v * 10) / 10}°C`);
		return typeof a.setpoint === 'number'
			? `${show(a.temperature)} · target ${show(a.setpoint)}`
			: show(a.temperature);
	};
	const faultOf = (t) => alerts.find((a) => a.device_id === t.device_id)?.reason ?? t.fault;
</script>

<div class="page">
	{#if trouble}<p class="bad" role="status">{trouble}</p>{/if}
	{#if busy}<p class="hc-quiet" role="status">{busy}</p>{/if}
	{#if failed}<p class="bad" role="alert">Couldn't link: {failed}</p>{/if}
	{#if said}<p class="good" role="status">{said}</p>{/if}

	{#if status === null}
		<p class="hc-quiet">Asking the plugin…</p>
	{:else}
		<section class="hc-card">
			<div class="hc-head">
				<h3>
					<span class="hc-dot" class:good={status.linked} class:bad={!status.linked}></span>
					{status.linked ? 'Signed in to NuHeat' : 'Not signed in to NuHeat'}
				</h3>
			</div>
			{#if status.linked}
				<div class="facts">
					{#if status.account}<span><span class="hc-label">Account</span> {status.account}</span>{/if}
					{#if status.linked_at}<span><span class="hc-label">Since</span> {day(status.linked_at)}</span>{/if}
				</div>
				<p class="hc-quiet">
					It stays signed in while homeCore runs. If homeCore is off for 15 days, sign in again.
				</p>
			{:else}
				<p class="hc-quiet">
					Sign in with your NuHeat account, the one the NuHeat app uses. You type your password on NuHeat's own
					site, never here.
				</p>
			{/if}

			{#if admin}
				<div class="hc-row">
					<button class="hc-pill go" disabled={!!busy} onclick={link}>
						{status.linked ? 'Sign in again' : 'Link NuHeat account'}
					</button>
					{#if status.linked}
						<button class="hc-pill" disabled={refreshing} onclick={refresh}>{refreshing ? 'Refreshing…' : 'Refresh now'}</button>
						<button class="hc-pill danger" onclick={signOut}>Sign out</button>
					{/if}
				</div>
				<details bind:open={pasting}>
					<summary class="hc-quiet">Can't come back to this page from NuHeat?</summary>
					<p class="hc-quiet">
						Sign in in a new tab instead. NuHeat then sends that tab to an address that won't load; that's expected.
						Copy the whole address from its address bar and paste it here.
					</p>
					<div class="hc-row">
						<button class="hc-pill" onclick={linkByPasting}>Open NuHeat in a new tab</button>
					</div>
					<div class="hc-row">
						<input class="hc-field paste" placeholder="http://127.0.0.1:8127/nuheat/callback?code=…" bind:value={pasted} aria-label="The address you landed on" />
						<button class="hc-pill go" disabled={!pasted.trim() || !!busy} onclick={() => finish({ pasted: pasted.trim() })}>Link</button>
					</div>
				</details>
			{:else}
				<p class="hc-quiet">An admin links the account.</p>
			{/if}
		</section>

		{#if status.linked}
			<section class="hc-card">
				<h3>Thermostats</h3>
				{#if (status.thermostats ?? []).length === 0}
					<p class="hc-quiet">None yet. They appear within a minute or two of signing in.</p>
				{:else}
					<table class="hc-table">
						<tbody>
							{#each status.thermostats as t (t.device_id)}
								{@const f = faultOf(t)}
								<tr>
									<td>
										<button class="link" onclick={() => hc.ui.openDevice(t.device_id)}>{t.name || t.serial}</button>
									</td>
									<td class="hc-num">{reading(deviceOf(t.device_id))}</td>
									<td class:bad={!!f}>{f ?? ''}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				{/if}
			</section>
		{/if}
	{/if}

	<p class="hc-quiet foot">
		This page comes with the NuHeat plugin. {status?.own_client
			? "It signs in with your own NuHeat client, from its settings."
			: "It signs in as homeCore's NuHeat client."}
	</p>
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: 16px;
	}
	h3 {
		margin: 0;
		font-size: 17px;
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.facts {
		display: flex;
		flex-wrap: wrap;
		gap: 6px 22px;
		font-size: 14px;
	}
	details {
		font-size: 14px;
	}
	summary {
		cursor: pointer;
		min-height: 32px;
		display: flex;
		align-items: center;
	}
	.paste {
		flex: 1;
		min-width: 0;
	}
	.link {
		border: 0;
		background: none;
		padding: 0;
		font: inherit;
		color: inherit;
		text-decoration: underline;
		cursor: pointer;
		text-align: left;
	}
	.bad {
		color: var(--alert);
	}
	.warn {
		color: var(--lit-ink);
		font-weight: 600;
	}
	.good {
		color: var(--ink);
		font-weight: 600;
	}
	.foot {
		font-size: 12px;
	}
</style>
