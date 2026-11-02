<svelte:options customElement={{ tag: 'hc-mysa-page', props: { host: { type: 'Object' } } }} />

<!-- hc-mysa's page: whether it is signed in to Mysa and until when, the
     thermostats it found, and the way to sign in. Signing in goes to Mysa's
     own site; Mysa sends the browser to homecore.io, which sends it back here
     with a code, and this page hands that to the plugin. Its settings are the
     app's generic form, below this page. -->
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

	// ---- coming back from Mysa (through homecore.io) ----
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
		busy = 'Checking your sign-in with Mysa…';
		failed = '';
		said = '';
		const r = await hc.action('complete_link', params);
		busy = '';
		if (r.ok && r.data.status !== 'error') {
			said =
				typeof r.data.thermostats === 'number'
					? `Linked. Mysa shared ${r.data.thermostats} thermostat${r.data.thermostats === 1 ? '' : 's'}.`
					: 'Linked.';
			pasting = false;
			pasted = '';
		} else failed = r.ok ? r.data.error : r.error;
		void load();
	}

	// ---- going to Mysa ----
	async function link() {
		busy = 'Opening Mysa…';
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

	// homecore.io sends Mysa's answer back only to a home network (the same
	// rule as the plugin's at_home), and only once Mysa has registered it.
	function atHome(loc) {
		const host = loc.hostname.replace(/^\[|\]$/g, '').toLowerCase();
		if (host === 'localhost' || host === '::1') return true;
		if (/^f[cd][0-9a-f]{0,2}:/.test(host)) return true;
		const m = host.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
		if (m) {
			const a = +m[1], b = +m[2];
			return a === 10 || a === 127 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
		}
		return /^[a-z0-9-]+(\.[a-z0-9-]+)*\.(local|home\.arpa|internal)$/.test(host);
	}
	const viaRelay = $derived(!!status?.relay_ready && atHome(location));

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
			title: 'Sign out of Mysa?',
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
	const daysLeft = (iso) => Math.ceil((Date.parse(iso) - now) / 86_400_000);
	const soon = $derived(status?.relink_due ? daysLeft(status.relink_due) <= 5 : false);

	const deviceOf = (id) => devices.find((d) => d.device_id === id);
	const reading = (d) => {
		const a = d?.attributes ?? {};
		if (typeof a.temperature !== 'number') return '—';
		const show = (v) => (hc?.units ? hc.units.show(v, '°C').text : `${Math.round(v * 10) / 10}°C`);
		if (a.hvac_mode === 'off') return `${show(a.temperature)} · off`;
		if (a.hvac_mode === 'auto' && typeof a.heat_setpoint === 'number' && typeof a.cool_setpoint === 'number')
			return `${show(a.temperature)} · heat ${show(a.heat_setpoint)}, cool ${show(a.cool_setpoint)}`;
		const target = a.hvac_mode === 'cool' ? a.cool_setpoint : a.heat_setpoint;
		return typeof target === 'number' ? `${show(a.temperature)} · target ${show(target)}` : show(a.temperature);
	};
	const noteOf = (t) =>
		alerts.find((a) => a.device_id === t.device_id)?.reason ??
		(t.connected === false ? 'Offline' : t.described === false ? 'Read-only for now' : '');
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
					<span class="hc-dot" class:good={status.linked && !soon} class:warn={soon} class:bad={!status.linked}></span>
					{!status.configured ? 'Waiting for a Mysa client id' : status.linked ? 'Signed in to Mysa' : 'Not signed in to Mysa'}
				</h3>
			</div>
			{#if !status.configured}
				<p class="hc-quiet">
					This build of the plugin has no Mysa client id. If Mysa issued you a client of your own, enter its id in the
					settings below.
				</p>
			{:else if status.linked}
				<div class="facts">
					{#if status.relink_due}
						<span class:warn={soon}
							><span class="hc-label">Sign in again by</span>
							{day(status.relink_due)}{#if soon}{' '}({daysLeft(status.relink_due) <= 0 ? 'now' : `${daysLeft(status.relink_due)} days`}){/if}</span
						>
					{/if}
				</div>
				<p class="hc-quiet">
					Mysa ends a sign-in 30 days after it's made. Signing in again takes a minute and starts another 30.
				</p>
			{:else}
				<p class="hc-quiet">
					Sign in with your Mysa account, the one the Mysa app uses, and choose Allow. You type your password on
					Mysa's own site, never here.
				</p>
			{/if}

			{#if admin && status.configured}
				<div class="hc-row">
					<button class="hc-pill go" disabled={!!busy} onclick={viaRelay ? link : linkByPasting}>
						{status.linked ? 'Sign in again' : 'Link Mysa account'}
					</button>
					{#if status.linked}
						<button class="hc-pill" disabled={refreshing} onclick={refresh}>{refreshing ? 'Refreshing…' : 'Refresh now'}</button>
						<button class="hc-pill danger" onclick={signOut}>Sign out</button>
					{/if}
				</div>
				{#if viaRelay}
				<details bind:open={pasting}>
					<summary class="hc-quiet">Can't come back to this page from Mysa?</summary>
					<p class="hc-quiet">
						Sign in in a new tab instead. Mysa then sends that tab to an address that won't load; that's expected.
						Copy the whole address from its address bar and paste it here within a minute: Mysa's code doesn't
						last longer.
					</p>
					<div class="hc-row">
						<button class="hc-pill" onclick={linkByPasting}>Open Mysa in a new tab</button>
					</div>
					<div class="hc-row">
						<input class="hc-field paste" placeholder="http://127.0.0.1:8127/mysa/callback?code=…" bind:value={pasted} aria-label="The address you landed on" />
						<button class="hc-pill go" disabled={!pasted.trim() || !!busy} onclick={() => finish({ pasted: pasted.trim() })}>Link</button>
					</div>
				</details>
				{:else}
					<p class="hc-quiet">
						Mysa opens in a new tab. After you sign in and choose Allow, it sends that tab to an address that won't
						load; that's expected. Copy the whole address from its address bar and paste it here within a minute:
						Mysa's code doesn't last longer.
					</p>
					{#if pasting}
						<div class="hc-row">
							<input class="hc-field paste" placeholder="http://127.0.0.1:8127/mysa/callback?code=…" bind:value={pasted} aria-label="The address you landed on" />
							<button class="hc-pill go" disabled={!pasted.trim() || !!busy} onclick={() => finish({ pasted: pasted.trim() })}>Link</button>
						</div>
					{/if}
				{/if}
			{:else if status.configured}
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
								{@const f = noteOf(t)}
								<tr>
									<td>
										<button class="link" onclick={() => hc.ui.openDevice(t.device_id)}>{t.name || t.mysa_id}</button>
									</td>
									<td class="hc-num">{reading(deviceOf(t.device_id))}</td>
									<td class:bad={!!f}>{f}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				{/if}
			</section>
		{/if}
	{/if}

	<p class="hc-quiet foot">
		This page comes with the Mysa plugin. {status?.own_client
			? "It signs in with your own Mysa client, from its settings."
			: "It signs in as homeCore's Mysa client."}
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
