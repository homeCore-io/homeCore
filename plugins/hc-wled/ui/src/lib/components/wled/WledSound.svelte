<!--
	How a WLED controller listens (admins): the AudioReactive usermod's gain,
	what it ignores, automatic gain, how quickly levels rise and fall, sound
	sync with other WLEDs — and the microphone, which takes a restart.
-->
<script lang="ts">
	import { ask } from '#lib/confirm.svelte.js';
	import { applySettings } from '#lib/wledSettings.js';

	type Audio = {
		enabled?: boolean;
		config?: { squelch?: number; gain?: number; AGC?: number };
		dynamics?: { limiter?: boolean; rise?: number; fall?: number };
		sync?: { port?: number; mode?: number };
		digitalmic?: { type?: number; pin?: number[] };
		analogmic?: { pin?: number };
	};
	let { deviceId, audio, reload }: { deviceId: string; audio: Audio; reload: () => Promise<void> } =
		$props();

	let msg = $state('');
	let busy = $state(false);
	async function put(part: Audio, restart = false) {
		busy = true;
		const err = await applySettings(deviceId, { um: { AudioReactive: part } }, restart);
		busy = false;
		msg = err ?? (restart ? 'Saved — the controller is restarting.' : 'Saved.');
		await reload();
	}
	const cfg = $derived(audio.config ?? {});
	const dyn = $derived(audio.dynamics ?? {});
	const mic = $derived(audio.digitalmic ?? {});
	const pins = $derived([...(mic.pin ?? []), -1, -1, -1, -1].slice(0, 4));
	const MICS = [
		'Analog (ADC)',
		'I²S',
		'ES7243',
		'SPH0654',
		'I²S with master clock',
		'I²S PDM',
		'ES8388'
	];
	const PIN_NAMES = ['Data (SD)', 'Word select (WS)', 'Clock (SCK)', 'Master clock'];
	async function setMic(change: { type?: number; pin?: number[] }) {
		if (
			!(await ask({
				title: 'Change the microphone? The controller restarts to take it.',
				go: 'Change and restart',
				stay: 'Not now'
			}))
		)
			return;
		void put({ digitalmic: { ...mic, ...change } }, true);
	}
</script>

<section class="wf">
	<div class="head">
		<h3>Listening</h3>
		<span class="num">AudioReactive</span>
	</div>
	<div class="cols">
		<label class="f"
			><span class="label">Gain · <span class="num">{cfg.gain ?? '—'}</span></span>
			<input
				type="range"
				min="1"
				max="255"
				value={cfg.gain ?? 60}
				disabled={busy}
				onchange={(e) => put({ config: { gain: Number(e.currentTarget.value) } })}
			/></label
		>
		<label class="f"
			><span class="label">Ignore sound below · <span class="num">{cfg.squelch ?? '—'}</span></span>
			<input
				type="range"
				min="0"
				max="255"
				value={cfg.squelch ?? 10}
				disabled={busy}
				onchange={(e) => put({ config: { squelch: Number(e.currentTarget.value) } })}
			/></label
		>
		<label class="f"
			><span class="label">Adjust the gain by itself</span>
			<select
				value={cfg.AGC ?? 0}
				disabled={busy}
				onchange={(e) => put({ config: { AGC: Number(e.currentTarget.value) } })}
			>
				<option value={0}>No</option>
				<option value={1}>Yes — normal</option>
				<option value={2}>Yes — vivid</option>
				<option value={3}>Yes — lazy</option>
			</select></label
		>
		<label class="f"
			><span class="label">Sound from other WLEDs</span>
			<select
				value={audio.sync?.mode ?? 0}
				disabled={busy}
				onchange={(e) => put({ sync: { mode: Number(e.currentTarget.value) } })}
			>
				<option value={0}>Off — its own microphone</option>
				<option value={1}>Send what it hears</option>
				<option value={2}>Listen to another’s</option>
			</select></label
		>
	</div>
	<div class="cols">
		<label class="c"
			><input
				type="checkbox"
				checked={dyn.limiter === true}
				disabled={busy}
				onchange={(e) => put({ dynamics: { limiter: e.currentTarget.checked } })}
			/>Smooth the levels</label
		>
		{#if dyn.limiter}
			<label class="f"
				><span class="label">Rise (ms)</span>
				<input
					type="number"
					min="0"
					max="5000"
					value={dyn.rise ?? 80}
					disabled={busy}
					onchange={(e) => put({ dynamics: { rise: Number(e.currentTarget.value) } })}
				/></label
			>
			<label class="f"
				><span class="label">Fall (ms)</span>
				<input
					type="number"
					min="0"
					max="5000"
					value={dyn.fall ?? 1400}
					disabled={busy}
					onchange={(e) => put({ dynamics: { fall: Number(e.currentTarget.value) } })}
				/></label
			>
		{/if}
	</div>
	<h4>Microphone <span class="num">· changes restart the controller</span></h4>
	<div class="cols">
		<label class="f"
			><span class="label">Kind</span>
			<select
				value={mic.type ?? 0}
				disabled={busy}
				onchange={(e) => setMic({ type: Number(e.currentTarget.value) })}
			>
				{#each MICS as m, n (m)}<option value={n}>{m}</option>{/each}
			</select></label
		>
		{#if (mic.type ?? 0) === 0}
			<label class="f"
				><span class="label">Pin</span>
				<input
					type="number"
					min="-1"
					max="48"
					value={audio.analogmic?.pin ?? -1}
					disabled={busy}
					onchange={async (e) => {
						const pin = Number(e.currentTarget.value);
						if (
							await ask({
								title: 'Change the microphone pin? The controller restarts to take it.',
								go: 'Change and restart',
								stay: 'Not now'
							})
						)
							void put({ analogmic: { pin } }, true);
					}}
				/></label
			>
		{:else}
			{#each PIN_NAMES as name, n (name)}
				{#if n < 3 || mic.type === 4 || mic.type === 6}
					<label class="f"
						><span class="label">{name}</span>
						<input
							type="number"
							min="-1"
							max="48"
							value={pins[n]}
							disabled={busy}
							onchange={(e) =>
								setMic({
									pin: pins.map((p, j) => (j === n ? Number(e.currentTarget.value) : p))
								})}
						/></label
					>
				{/if}
			{/each}
		{/if}
	</div>
	<p class="quiet">A pin of −1 is unused.</p>
	{#if msg}<p class="quiet" role="status">{msg}</p>{/if}
</section>
