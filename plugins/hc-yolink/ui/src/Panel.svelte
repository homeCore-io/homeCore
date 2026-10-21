<svelte:options
	customElement={{
		tag: 'hc-yolink-panel',
		props: { host: { type: 'Object' }, deviceId: { type: 'String', attribute: 'device-id' } }
	}}
/>

<!-- A YoLink device's panel. A lock: Lock (its state, Lock / Unlock, door,
     battery, last problem, its own settings) and History. A temperature
     sensor: "Tell me when" — homeCore alert rules — and how it reports. A
     leak sensor: dry or wet, when it was last wet, how it reports, its own
     temperature, whether its probe works. -->
<script>
	import History from './lib/History.svelte';
	import LeakView from './lib/LeakView.svelte';
	import LockTab from './lib/LockTab.svelte';
	import TempView from './lib/TempView.svelte';
	import { adopt, becameTrue, historyFrom, isLeak, isLock, isTemp, statusOf } from './lib/yolink.js';

	let { host: hc, deviceId } = $props();
	const me = $host();
	$effect(() => adopt(me, hc));

	let devices = $state([]);
	$effect(() => hc?.devices.subscribe((list) => (devices = list)));
	const dev = $derived(devices.find((d) => d.device_id === deviceId));
	const canRun = $derived(!!hc?.me.can('devices:command'));

	// How it reports, from the plugin's status.
	let row = $state(null);
	let status = $state(null);
	async function readStatus() {
		const r = await hc.query('status');
		if (r.ok) {
			status = r.data;
			row = statusOf(r.data, deviceId) ?? null;
		}
	}
	$effect(() => {
		if (!hc || !deviceId) return;
		void readStatus();
		const t = setInterval(readStatus, 20000);
		return () => clearInterval(t);
	});

	// A lock's history: its lock events and its door, kept by homeCore, with
	// who asked from the recent event log.
	let history = $state(null);
	let tick = $state(Date.now());
	async function readHistory() {
		const [events, door, log] = await Promise.all([
			hc.devices.history(deviceId, 'last_lock_event', { limit: 300 }),
			hc.devices.history(deviceId, 'open', { limit: 300 }),
			hc
				.fetch(`/events?device_id=${encodeURIComponent(deviceId)}&type=device_state_changed&limit=300`)
				.then((r) => (r.ok ? r.json() : []))
				.catch(() => [])
		]);
		history = historyFrom({
			lockEvents: events.ok ? events.data.map((x) => x.value) : [],
			doorOpened: door.ok ? becameTrue(door.data) : [],
			log: Array.isArray(log) ? log : []
		});
		tick = Date.now();
	}
	const lock = $derived(isLock(dev));
	const lastAt = $derived(dev?.attributes?.last_lock_event?.at);
	const doorNow = $derived(dev?.attributes?.open);
	$effect(() => {
		void lastAt;
		void doorNow;
		if (hc && deviceId && lock) void readHistory();
	});

	const TABS = [
		['lock', 'Lock'],
		['history', 'History']
	];
	let tab = $state('lock');
</script>

{#if dev}
	<div class="panel">
		{#if lock}
			<div class="seg" role="tablist" aria-label="Lock">
				{#each TABS as [id, name] (id)}
					<button role="tab" class:on={tab === id} aria-selected={tab === id} onclick={() => (tab = id)}>{name}</button>
				{/each}
			</div>
			{#if tab === 'lock'}
				<LockTab {hc} {dev} {canRun} />
			{:else}
				<History {history} {tick} />
			{/if}
		{:else if isLeak(dev)}
			<LeakView {hc} {dev} {row} {status} />
		{:else if isTemp(dev)}
			<TempView {hc} {dev} {row} {status} />
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
	}
	.seg button.on {
		background: var(--surface);
		color: var(--ink);
		box-shadow: 0 1px 2px rgba(0, 0, 0, 0.08);
	}
</style>
