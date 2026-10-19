<!--
	A WLED controller's own timers (admins): each at a time of day, or at
	sunrise or sunset with an offset; on which days and between which dates;
	the preset it applies. They run even when homeCore is down.
-->
<script lang="ts">
	import type { Preset } from '#lib/wled.js';
	import {
		applySettings,
		DAYS,
		emptied,
		isEmptyTimer,
		isSun,
		SUNRISE,
		SUNSET,
		timersToSend,
		type Timer
	} from '#lib/wledSettings.js';

	let {
		deviceId,
		timers,
		presets,
		reload
	}: {
		deviceId: string;
		timers: Timer[];
		presets: Preset[];
		reload: () => Promise<void>;
	} = $props();

	let msg = $state('');
	let busy = $state(false);
	const shown = $derived(timers.map((t, i) => ({ t, i })).filter(({ t }) => !isEmptyTimer(t)));
	const regular = $derived(timers.filter((t) => !isSun(t)).length);

	async function save(list: Timer[], said = 'Saved.') {
		busy = true;
		const err = await applySettings(deviceId, { timers: { ins: timersToSend(list) } });
		busy = false;
		msg = err ?? said;
		await reload();
	}
	const change = (i: number, patch: Partial<Timer>) =>
		save(timers.map((t, j) => (j === i ? { ...t, ...patch } : t)));
	function add() {
		const fresh: Timer = {
			en: 1,
			hour: 7,
			min: 0,
			macro: presets[0]?.id ?? 0,
			dow: 127,
			start: { mon: 1, day: 1 },
			end: { mon: 12, day: 31 }
		};
		// Into an empty slot WLED kept, else a new one.
		const gap = timers.findIndex(isEmptyTimer);
		void save(
			gap >= 0 ? timers.map((t, j) => (j === gap ? fresh : t)) : [...timers, fresh],
			'Added.'
		);
	}
	const remove = (i: number) =>
		save(
			timers.map((t, j) => (j === i ? emptied(t) : t)),
			'Removed.'
		);

	const hhmm = (t: Timer) =>
		`${String(t.hour).padStart(2, '0')}:${String(Math.max(0, t.min)).padStart(2, '0')}`;
	const dayOn = (t: Timer, d: number) => ((t.dow ?? 127) & (1 << d)) !== 0;
	const MONTHS = [
		'Jan',
		'Feb',
		'Mar',
		'Apr',
		'May',
		'Jun',
		'Jul',
		'Aug',
		'Sep',
		'Oct',
		'Nov',
		'Dec'
	];
</script>

<section class="wf">
	<div class="head">
		<h3>Timers on the controller</h3>
		<button class="pill" disabled={busy || regular >= 8 || !presets.length} onclick={add}
			>Add a timer</button
		>
	</div>
	<p class="quiet">
		These run even when homeCore is down. Automations are usually the better place. Up to eight at a
		time of day, plus one at sunrise and one at sunset.
	</p>
	{#each shown as { t, i } (i)}
		<div class="item">
			<div class="cols">
				<label class="c"
					><input
						type="checkbox"
						checked={t.en === 1}
						disabled={busy}
						onchange={(e) => change(i, { en: e.currentTarget.checked ? 1 : 0 })}
					/><b>{t.hour === SUNRISE ? 'At sunrise' : t.hour === SUNSET ? 'At sunset' : 'At'}</b
					></label
				>
				{#if isSun(t)}
					<label class="f"
						><span class="label">Minutes {t.min < 0 ? 'before' : 'after'}</span>
						<input
							type="number"
							min="-59"
							max="59"
							value={t.min}
							disabled={busy}
							onchange={(e) =>
								change(i, { min: Math.max(-59, Math.min(59, Number(e.currentTarget.value))) })}
						/></label
					>
				{:else}
					<label class="f"
						><span class="label">Time</span>
						<input
							type="time"
							value={hhmm(t)}
							disabled={busy}
							onchange={(e) => {
								const [h, m] = e.currentTarget.value.split(':').map(Number);
								if (Number.isInteger(h) && Number.isInteger(m)) change(i, { hour: h, min: m });
							}}
						/></label
					>
				{/if}
				<label class="f"
					><span class="label">Preset</span>
					<select
						value={t.macro}
						disabled={busy}
						onchange={(e) => change(i, { macro: Number(e.currentTarget.value) })}
					>
						<option value={0}>No preset</option>
						{#each presets as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
					</select></label
				>
			</div>
			<div class="days" role="group" aria-label="Days">
				{#each DAYS as d, n (d)}
					<button
						aria-pressed={dayOn(t, n)}
						disabled={busy}
						onclick={() => change(i, { dow: (t.dow ?? 127) ^ (1 << n) })}>{d}</button
					>
				{/each}
			</div>
			<div class="cols">
				{#each [['From', 'start', { mon: 1, day: 1 }], ['Until', 'end', { mon: 12, day: 31 }]] as const as [label, key, dflt] (key)}
					{@const v = t[key] ?? dflt}
					<div class="cols">
						<label class="f"
							><span class="label">{label}</span>
							<select
								value={v.mon}
								disabled={busy}
								onchange={(e) => change(i, { [key]: { ...v, mon: Number(e.currentTarget.value) } })}
							>
								{#each MONTHS as m, n (m)}<option value={n + 1}>{m}</option>{/each}
							</select></label
						>
						<label class="f"
							><span class="label">Day</span>
							<input
								type="number"
								min="1"
								max="31"
								value={v.day}
								disabled={busy}
								onchange={(e) => change(i, { [key]: { ...v, day: Number(e.currentTarget.value) } })}
							/></label
						>
					</div>
				{/each}
			</div>
			{#if !isSun(t)}<div class="buttons">
					<button class="pill danger" disabled={busy} onclick={() => remove(i)}>Remove</button>
				</div>{/if}
		</div>
	{/each}
	{#if msg}<p class="quiet" role="status">{msg}</p>{/if}
</section>
