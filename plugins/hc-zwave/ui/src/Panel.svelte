<svelte:options
	customElement={{
		tag: 'hc-zwave-panel',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A Z-Wave device's panel. A lock: Lock, Codes, History and Health (and
     Settings, if it has any) — it draws its own lock control. Anything else
     keeps the app's own control above and adds Health and Settings. -->
<script>
	import Codes from './lib/Codes.svelte';
	import Health from './lib/Health.svelte';
	import History from './lib/History.svelte';
	import LockTab from './lib/LockTab.svelte';
	import Settings from './lib/Settings.svelte';
	import { adopt, historyFrom, lastUsedBySlot, slotNames } from './lib/zwave.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const isLock = $derived(dev?.device_type === 'lock' || dev?.attributes?.locked !== undefined);
	const admin = $derived(!!(hc?.me.isAdmin || hc?.me.can('plugins:write')));
	const canRun = $derived(!!hc?.me.can('devices:command'));

	// Its health, from the plugin's status.
	let node = $state(null);
	async function readStatus() {
		const r = await hc.query('status');
		if (r.ok) node = (r.data?.nodes ?? []).find((n) => n.device_id === deviceId) ?? null;
	}
	$effect(() => {
		if (!hc || !deviceId) return;
		void readStatus();
		const t = setInterval(readStatus, 15000);
		return () => clearInterval(t);
	});

	// A lock's codes and history.
	let codes = $state({ names: {}, slots: [] });
	let history = $state(null);
	let tick = $state(Date.now());
	async function readCodes() {
		codes = await slotNames(hc, deviceId);
	}
	// After a change, again once the lock has had time to report it.
	function codesChanged() {
		void readCodes();
		setTimeout(readCodes, 2500);
	}
	// The lock's own history of `last_lock_event` (kept by homeCore), with who
	// asked — an automation, a person — from the recent event log where it
	// still has them.
	async function readHistory() {
		let events = [];
		try {
			const r = await hc.fetch(`/events?device_id=${encodeURIComponent(deviceId)}&type=device_state_changed&limit=500`);
			const rows = r.ok ? await r.json() : [];
			events = Array.isArray(rows) ? rows : [];
		} catch {
			events = [];
		}
		const kept = await hc.devices.history(deviceId, 'last_lock_event', { limit: 300 });
		const past = (kept.ok ? kept.data : [])
			.filter((x) => x.value && typeof x.value === 'object')
			.map((x) => ({ event: { changed: ['last_lock_event'], current: { last_lock_event: x.value } } }));
		history = historyFrom([...events, ...past], codes.names);
		tick = Date.now();
	}
	$effect(() => {
		if (!hc || !deviceId || !isLock) return;
		void readCodes().then(readHistory);
	});
	// A new lock event: read the history again.
	const lastAt = $derived(dev?.attributes?.last_lock_event?.at);
	$effect(() => {
		if (lastAt && isLock) void readHistory();
	});

	let hasParams = $state(false);
	const tabs = $derived(
		isLock
			? [['lock', 'Lock'], ['codes', 'Codes'], ['history', 'History'], ['health', 'Health'], ...(hasParams ? [['settings', 'Settings']] : [])]
			: [['health', 'Health'], ['settings', 'Settings']]
	);
	let picked = $state(null);
	const tab = $derived(picked && tabs.some(([id]) => id === picked) ? picked : tabs[0][0]);
	// Whether a lock has settings at all, to offer the tab.
	$effect(() => {
		if (!hc || !deviceId || !isLock) return;
		void hc.query('get_config_params', { device_id: deviceId }).then((r) => (hasParams = !!(r.ok && r.data?.params?.length)));
	});
</script>

{#if dev}
	<div class="panel" class:off={dev.available === false}>
		<div class="seg" role="tablist" aria-label="Z-Wave">
			{#each tabs as [id, name] (id)}
				<button role="tab" class:on={tab === id} aria-selected={tab === id} onclick={() => (picked = id)}>{name}</button>
			{/each}
		</div>
		{#if tab === 'lock'}
			<LockTab {hc} {dev} names={codes.names} {canRun} {admin} />
		{:else if tab === 'codes'}
			<Codes {hc} {deviceId} slots={codes.slots} names={codes.names} lastUsed={lastUsedBySlot(history ?? [])} {admin} onchanged={codesChanged} />
		{:else if tab === 'history'}
			<History {history} {tick} />
		{:else if tab === 'health'}
			<Health {hc} {deviceId} {node} name={hc.devices.label(dev)} {admin} onchanged={readStatus} />
		{:else}
			<Settings {hc} {deviceId} {dev} {admin} onloaded={(n) => (hasParams = n > 0)} />
		{/if}
	</div>
{/if}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		gap: 18px;
	}
	.seg {
		display: flex;
		gap: 6px;
		padding: 4px;
		border-radius: 12px;
		background: var(--nav-on, var(--line));
	}
	.seg button {
		flex: 1;
		min-height: 40px;
		border: none;
		background: none;
		border-radius: 9px;
		font-weight: 600;
		font-size: 14px;
		color: var(--ink-muted);
		padding: 0 4px;
	}
	.seg button.on {
		background: var(--surface);
		color: var(--ink);
		box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
	}
</style>
