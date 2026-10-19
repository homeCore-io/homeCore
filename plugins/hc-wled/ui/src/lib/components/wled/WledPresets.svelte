<!--
	A WLED controller's presets: each as what it looks like, applied with a
	tap; and for the one chosen, its name and quick label, what it holds,
	saving what's running into it, starting the controller with it, deleting
	it — and which automations apply it, so deleting one never surprises one.
	Admins back the presets up, restore them, and copy them to another
	controller.
-->
<script lang="ts">
	import { host } from '#lib/host.js';
	import type { CoreDevice } from '#lib/devices.js';
	import { commands } from '#lib/commands.svelte.js';
	import { devices } from '#lib/devices.svelte.js';
	import { session } from '#lib/me.svelte.js';
	import { ask } from '#lib/confirm.svelte.js';
	import { runAction } from '#lib/pluginAdmin.js';
	import {
		effectNames,
		isWled,
		nameOf,
		paletteNames,
		presetCss,
		presetsOf,
		WLED,
		type Catalog
	} from '#lib/wled.js';

	let {
		device,
		catalog,
		disabled
	}: { device: CoreDevice; catalog: Catalog | undefined; disabled: boolean } = $props();

	const id = $derived(device.device_id);
	const all = $derived(presetsOf(device));
	const presets = $derived(all.filter((p) => p.kind !== 'playlist'));
	let q = $state('');
	let order = $state<'slot' | 'name'>('slot');
	const shown = $derived(
		presets
			.filter((p) => p.name.toLowerCase().includes(q.trim().toLowerCase()))
			.sort((a, b) => (order === 'name' ? a.name.localeCompare(b.name) : a.id - b.id))
	);
	let chosenId = $state<number | undefined>(undefined);
	const chosen = $derived(
		presets.find((p) => p.id === (chosenId ?? device.attributes.preset_id)) ?? presets[0]
	);
	const running = $derived(device.attributes.preset_id);
	const powerOn = $derived(device.attributes.power_on_preset_id);
	const effectName = (fx: unknown) =>
		catalog?.effects.find((e) => e.id === fx)?.name ?? nameOf(effectNames(device), fx);
	const paletteName = (p: unknown) =>
		catalog?.palettes.find((x) => x.id === p)?.name ?? nameOf(paletteNames(device), p);

	// ---- which automations apply it ----
	let rules = $state<
		{ id: string; name: string; actions?: unknown; trigger?: unknown; triggers?: unknown }[]
	>([]);
	$effect(() => {
		void host()
			.fetch('/automations')
			.then((r) => (r.ok ? r.json() : []))
			.then((list) => (rules = Array.isArray(list) ? list : []))
			.catch(() => (rules = []));
	});
	function mentions(v: unknown, preset: number): boolean {
		if (Array.isArray(v)) return v.some((x) => mentions(x, preset));
		if (v === null || typeof v !== 'object') return false;
		const o = v as Record<string, unknown>;
		const sds = o.SetDeviceState as
			{ device_id?: unknown; state?: Record<string, unknown> } | undefined;
		if (sds && sds.device_id === id && sds.state) {
			const s = sds.state;
			if ([s.preset_id, s.preset, s.apply_preset, s.playlist_id].includes(preset)) return true;
		}
		return Object.values(o).some((x) => mentions(x, preset));
	}
	const usedBy = $derived(
		chosen ? rules.filter((r) => mentions([r.actions, r.trigger, r.triggers], chosen.id)) : []
	);

	// ---- changing one ----
	let includeBri = $state(true);
	let includeSegs = $state(false);
	let selectedOnly = $state(false);
	const rename = (name: string, quick: string) =>
		chosen &&
		void commands.act(id, 'rename_preset', { preset_id: chosen.id, name, quick_label: quick });
	async function remove() {
		if (!chosen) return;
		const why = usedBy.length
			? ` ${usedBy.length} automation${usedBy.length === 1 ? ' applies' : 's apply'} it.`
			: '';
		if (
			!(await ask({
				title: `Delete “${chosen.name}” from the controller? It can't be undone.${why}`,
				go: 'Delete',
				stay: 'Keep it'
			}))
		)
			return;
		void commands.act(id, 'delete_preset', { delete_preset: chosen.id });
		chosenId = undefined;
	}

	// ---- admins: backups and other controllers ----
	const others = $derived(
		Object.values(devices.byId).filter((d) => isWled(d) && d.device_id !== id)
	);
	let copyTo = $state('');
	let message = $state('');
	async function backup() {
		const r = await runAction(WLED, 'backup', { device_id: id });
		if (!r.ok) return void (message = r.error);
		if (r.data.kind !== 'reply') return;
		const doc = (r.data.reply as { backup?: unknown }).backup;
		const blob = new Blob([JSON.stringify(doc, null, 2)], { type: 'application/json' });
		const a = document.createElement('a');
		a.href = URL.createObjectURL(blob);
		a.download = `${id}-presets-${new Date().toISOString().slice(0, 10)}.json`;
		a.click();
		URL.revokeObjectURL(a.href);
		message = 'Backed up — and kept with the plugin too.';
	}
	async function restore(file: File) {
		let doc: unknown;
		try {
			doc = JSON.parse(await file.text());
		} catch {
			return void (message = "That file isn't a backup.");
		}
		if (
			!(await ask({
				title: `Replace this controller's presets with the ones in ${file.name}?`,
				go: 'Restore',
				stay: 'Not now'
			}))
		)
			return;
		const r = await runAction(WLED, 'restore', {
			device_id: id,
			backup: doc,
			presets: true,
			settings: false
		});
		message = r.ok ? 'Presets restored.' : r.error;
	}
	// One preset: a duplicate here, or onto another controller.
	let oneTo = $state('');
	let oneMsg = $state('');
	async function copyOne() {
		if (!chosen || !oneTo) return;
		const here = oneTo === id;
		const to = here ? device : others.find((o) => o.device_id === oneTo);
		if (!to) return;
		const r = await runAction(WLED, 'copy_preset', {
			from_device_id: id,
			to_device_id: to.device_id,
			preset_id: chosen.id
		});
		if (!r.ok) return void (oneMsg = r.error);
		const at = r.data.kind === 'reply' ? (r.data.reply as { preset_id?: number }).preset_id : '';
		oneMsg = here
			? `Duplicated “${chosen.name}” into slot ${at}.`
			: `Copied “${chosen.name}” to ${to.name_override ?? to.name}, slot ${at}.`;
		oneTo = '';
	}
	async function copy() {
		const to = others.find((o) => o.device_id === copyTo);
		if (!to) return;
		if (
			!(await ask({
				title: `Replace ${to.name_override ?? to.name}'s presets with this controller's?`,
				go: 'Copy',
				stay: 'Not now'
			}))
		)
			return;
		const r = await runAction(WLED, 'copy_presets', {
			from_device_id: id,
			to_device_id: to.device_id
		});
		message = r.ok ? `Copied to ${to.name_override ?? to.name}.` : r.error;
	}
</script>

<div class="presets">
	<div class="tools">
		<input type="search" placeholder="Find a preset" aria-label="Find a preset" bind:value={q} />
		<div class="chips" role="radiogroup" aria-label="Order">
			<button role="radio" aria-checked={order === 'slot'} onclick={() => (order = 'slot')}
				>By slot</button
			>
			<button role="radio" aria-checked={order === 'name'} onclick={() => (order = 'name')}
				>By name</button
			>
		</div>
		<span class="num">{presets.length} of 250 slots</span>
	</div>

	<div class="cols">
		<div class="grid">
			{#each shown as p (p.id)}
				<div class="card" class:on={p.id === chosen?.id}>
					<button class="pick" onclick={() => (chosenId = p.id)}>
						<span class="sw" style:background={presetCss(p)}></span>
						<span class="row"><b>{p.name}</b><span class="num">{p.id}</span></span>
						<span class="num small">
							{p.kind === 'command'
								? 'a command'
								: (effectName(p.effect_id) ?? '')}{p.brightness_pct !== undefined
								? ` · ${p.brightness_pct}%`
								: ''}{p.quick_label ? ` · ${p.quick_label}` : ''}
						</span>
						{#if p.id === running}<span class="badge">Running</span>{/if}
						{#if p.id === powerOn}<span class="badge">At power-on</span>{/if}
					</button>
					<button
						class="apply"
						{disabled}
						aria-label={`Apply ${p.name}`}
						onclick={() => commands.act(id, 'apply_preset', { preset_id: p.id })}>Apply</button
					>
				</div>
			{/each}
			{#if presets.length === 0}<p class="quiet">
					No presets yet — save a look from the Look tab.
				</p>{/if}
		</div>

		{#if chosen}
			{@const p = chosen}
			<section class="editor">
				<div class="sw big" style:background={presetCss(p)}></div>
				<button
					class="primary"
					{disabled}
					onclick={() => commands.act(id, 'apply_preset', { preset_id: p.id })}>Apply now</button
				>
				{#key p.id}
					<label>
						<span class="label">Name</span>
						<input
							value={p.name}
							maxlength="32"
							{disabled}
							onchange={(e) => rename(e.currentTarget.value, p.quick_label ?? '')}
						/>
					</label>
					<label>
						<span class="label">Quick label</span>
						<input
							value={p.quick_label ?? ''}
							maxlength="2"
							placeholder="2 letters or an emoji"
							{disabled}
							onchange={(e) => rename(p.name, e.currentTarget.value)}
						/>
					</label>
				{/key}
				<div class="holds">
					<span class="label">What it holds · slot {p.id}</span>
					{#if p.kind === 'command'}
						<span>A command, not a look</span>
					{:else}
						<span
							>{effectName(p.effect_id) ?? 'An effect'}{p.palette_id !== undefined
								? ` · palette ${paletteName(p.palette_id) ?? p.palette_id}`
								: ''}{p.brightness_pct !== undefined ? ` · ${p.brightness_pct}%` : ''} · {p.segments ??
								1} segment{p.segments === 1 ? '' : 's'}</span
						>
					{/if}
					<label class="check"
						><input type="checkbox" bind:checked={includeBri} /> Include brightness</label
					>
					<label class="check"
						><input type="checkbox" bind:checked={includeSegs} /> Include segment layout</label
					>
					<label class="check"
						><input type="checkbox" bind:checked={selectedOnly} /> Only the selected segments</label
					>
					<button
						class="pill"
						{disabled}
						onclick={() =>
							commands.act(id, 'save_preset', {
								save_preset: p.id,
								preset_name: p.name,
								preset_quick_label: p.quick_label ?? '',
								include_brightness: includeBri,
								include_segments: includeSegs,
								selected_only: selectedOnly
							})}>Replace with what's running</button
					>
				</div>
				<label class="check">
					<input
						type="checkbox"
						checked={p.id === powerOn}
						{disabled}
						onchange={(e) =>
							commands.act(id, 'set_power_on_preset', {
								preset_id: e.currentTarget.checked ? p.id : 0
							})}
					/>
					Start with this when the controller powers on
				</label>
				<div class="used">
					<span class="label">Used by</span>
					{#if usedBy.length === 0}<span class="quiet small">No automation applies it.</span>{/if}
					{#each usedBy as r (r.id)}<a href={`/settings/automations/${r.id}`}>{r.name}</a>{/each}
				</div>
				<div class="row-buttons">
					{#if session.isAdmin}
						<select bind:value={oneTo} aria-label="Copy this preset">
							<option value="">Copy it…</option>
							<option value={id}>Duplicate it here</option>
							{#each others as o (o.device_id)}<option value={o.device_id}
									>To {o.name_override ?? o.name}</option
								>{/each}
						</select>
						<button class="pill" disabled={!oneTo} onclick={copyOne}>Copy</button>
					{/if}
					<button class="pill danger" {disabled} onclick={remove}>Delete</button>
				</div>
				{#if oneMsg}<p class="quiet small" role="status">{oneMsg}</p>{/if}
			</section>
		{/if}
	</div>

	{#if session.isAdmin}
		<section class="admin">
			<span class="label">All of this controller's presets</span>
			<div class="row-buttons">
				<button class="pill" onclick={backup}>Back up</button>
				<label class="pill file"
					>Restore from a backup…<input
						type="file"
						accept="application/json,.json"
						onchange={(e) => {
							const f = e.currentTarget.files?.[0];
							if (f) void restore(f);
							e.currentTarget.value = '';
						}}
					/></label
				>
				{#if others.length}
					<select bind:value={copyTo} aria-label="Copy them to">
						<option value="">Copy them to…</option>
						{#each others as o (o.device_id)}<option value={o.device_id}
								>{o.name_override ?? o.name}</option
							>{/each}
					</select>
					<button class="pill" disabled={!copyTo} onclick={copy}>Copy</button>
				{/if}
			</div>
			{#if message}<p class="quiet small" role="status">{message}</p>{/if}
		</section>
	{/if}
</div>

<style>
	.presets {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}
	.tools {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.tools input,
	.editor input,
	select {
		font: inherit;
		min-height: 40px;
		border-radius: 9px;
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		padding: 0 10px;
		min-width: 0;
	}
	.chips {
		display: flex;
		gap: 4px;
	}
	.chips button {
		font: inherit;
		font-size: 14px;
		min-height: 36px;
		padding: 0 12px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
	}
	.chips button[aria-checked='true'] {
		border-color: var(--accent);
		background: var(--lit-bg);
		color: var(--lit-ink);
		font-weight: 600;
	}
	.cols {
		display: flex;
		flex-wrap: wrap;
		gap: 16px;
		align-items: flex-start;
	}
	.grid {
		flex: 2 1 300px;
		min-width: 0;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
		gap: 10px;
	}
	.card {
		display: flex;
		flex-direction: column;
		border: 2px solid var(--card-line);
		border-radius: var(--radius);
		background: var(--surface);
		overflow: hidden;
	}
	.card.on {
		border-color: var(--accent);
	}
	.pick {
		font: inherit;
		color: var(--ink);
		text-align: left;
		border: none;
		background: none;
		padding: 10px;
		display: flex;
		flex-direction: column;
		gap: 6px;
		align-items: stretch;
	}
	.apply {
		font: inherit;
		font-size: 14px;
		min-height: 36px;
		border: none;
		border-top: 1px solid var(--card-line);
		background: var(--surface-2);
		color: var(--ink);
	}
	.sw {
		display: block;
		height: 30px;
		border-radius: 8px;
	}
	.sw.big {
		height: 44px;
	}
	.row {
		display: flex;
		justify-content: space-between;
		gap: 6px;
		align-items: baseline;
	}
	.badge {
		align-self: flex-start;
		font-size: 12px;
		font-weight: 600;
		color: var(--lit-ink);
		background: var(--lit-bg);
		border-radius: 6px;
		padding: 2px 8px;
	}
	.editor {
		flex: 1 1 260px;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 12px;
		padding: 14px;
		border: 1px solid var(--card-line);
		border-radius: var(--radius);
		background: var(--surface);
	}
	.holds,
	.used,
	.admin {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.holds {
		padding: 10px;
		border-radius: 11px;
		background: var(--sheet);
		border: 1px solid var(--card-line);
	}
	label {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.check {
		flex-direction: row;
		align-items: center;
		gap: 8px;
		min-height: 36px;
		font-size: 14px;
	}
	.check input {
		width: 18px;
		height: 18px;
		accent-color: var(--accent);
	}
	.label {
		font-size: 12px;
		font-weight: 600;
		color: var(--ink-muted);
	}
	.num {
		font-family: var(--font-num);
		font-size: 13px;
		color: var(--ink-muted);
	}
	.small {
		font-size: 12px;
	}
	.quiet {
		color: var(--ink-muted);
		margin: 0;
	}
	.primary {
		font: inherit;
		font-weight: 600;
		min-height: var(--target);
		border-radius: var(--radius-chip);
		border: none;
		background: var(--accent);
		color: var(--accent-ink);
	}
	.pill {
		font: inherit;
		font-size: 14px;
		min-height: 40px;
		padding: 0 14px;
		border-radius: var(--radius-chip);
		border: 1px solid var(--line);
		background: var(--surface);
		color: var(--ink);
		align-self: flex-start;
		display: inline-flex;
		align-items: center;
	}
	.pill.danger {
		color: var(--alert);
		border-color: var(--alert-line);
	}
	.file {
		position: relative;
		flex-direction: row;
	}
	.file input {
		position: absolute;
		inset: 0;
		opacity: 0;
	}
	.row-buttons {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	button:disabled {
		opacity: 0.55;
	}
</style>
