<!--
	The "install WLED updates by itself, at night" choice (admins). Turning it
	on writes an ordinary automation, linked here, and turns on the alert it
	acts on; turning it off switches that automation off and keeps it.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { alertKinds } from '#lib/alertKinds.svelte.js';
	import { createRule, loadRules, setEnabled } from '#lib/ruleAdmin.js';
	import type { Rule } from '#lib/ruleWords.js';
	import { autoUpdateRule, findAutoUpdate } from '#lib/wledAutoUpdate.js';
	import { WLED } from '#lib/wled.js';

	let rule = $state<Rule | undefined>(undefined);
	let loaded = $state(false);
	let busy = $state(false);
	let msg = $state('');
	async function load() {
		const r = await loadRules();
		rule = r.ok ? findAutoUpdate(r.data) : undefined;
		loaded = r.ok;
		if (!r.ok) msg = r.error;
	}
	onMount(() => {
		alertKinds.ensure();
		void load();
	});
	const on = $derived(rule?.enabled === true);

	async function turn(want: boolean) {
		busy = true;
		msg = '';
		if (want) {
			const kind = alertKinds.list.find(
				(k) => k.plugin_id === WLED && k.topic === 'firmware_update'
			);
			if (kind && !kind.enabled) {
				const err = await alertKinds.put(WLED, 'firmware_update', {
					enabled: true,
					notify: kind.notify ?? null
				});
				if (err) {
					busy = false;
					return void (msg = err);
				}
			}
			const r = rule ? await setEnabled(rule.id, true) : await createRule(autoUpdateRule());
			if (!r.ok) msg = r.error;
		} else if (rule) {
			const r = await setEnabled(rule.id, false);
			if (!r.ok) msg = r.error;
		}
		await load();
		busy = false;
	}
</script>

{#if loaded}
	<div class="auto">
		<label class="c">
			<input
				type="checkbox"
				checked={on}
				disabled={busy}
				onchange={(e) => turn(e.currentTarget.checked)}
			/>
			Install WLED updates by itself, at 03:00, one controller at a time
		</label>
		<p class="quiet">
			{#if rule}
				It’s the automation <a href={`/settings/automations/${rule.id}`}>{rule.name}</a>{on
					? ''
					: ', switched off'} — open it to change the time or what it does.
			{:else}
				Off unless turned on. Turning it on adds an ordinary automation you can open, change or
				switch off, and turns on the “Firmware update available” alert it acts on.
			{/if}
		</p>
		{#if msg}<p class="quiet" role="status">{msg}</p>{/if}
	</div>
{/if}

<style>
	.auto {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.c {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 40px;
	}
	.c input {
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.quiet {
		margin: 0;
		font-size: 13px;
		color: var(--ink-muted);
	}
</style>
