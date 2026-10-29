/**
 * A homeCore plugin's own screens — the `host` interface, version 1.
 *
 * A plugin carries web components (custom elements) beside its program — a
 * built `ui/` folder with a `manifest.json` — and serves them itself
 * (`ManagementHandle::with_ui_dir` in the Rust SDK); homeCore passes the
 * files through. The web app loads the module and creates each element where
 * the manifest says it goes, then sets:
 *
 * - `host` — this object, on every element;
 * - `deviceId` — on a `device.panel` element: the device being shown;
 * - `deviceIds` and `value` — on a `rule.step` element: the devices the step
 *   sets (one, or several at once) and what it sets the first to (its
 *   `state`). It answers with a `change` event whose `detail` is
 *   `{ action }`: the step as homeCore keeps it — a `SetDeviceState`, or a
 *   `Parallel` of them for several devices (each may be set differently) —
 *   on the plugin's own devices only.
 *
 * - `form` — on a `device.card` or `dashboard.card` element: the size it is
 *   drawn at, `compact` | `standard` | `large` (two columns), one of the
 *   sizes its slot declares (the first is its size when first added); it may
 *   change while shown;
 * - `deviceId` — on a `device.card` element, too: the device it is the card of;
 * - `config` — on a `dashboard.card` element: this card's settings, as its
 *   settings element last made them (`{}` until then). The same card can be
 *   on several pages, each with its own settings.
 *
 * A `dashboard.card` slot may name a `settings` element (in the same
 * module): it is given `host` and `config`, and answers with a `change`
 * event whose `detail` is `{ config }` — saved with the page, by the app.
 *
 * A `device.card` element's class may have a static `accepts(device)` —
 * whether it draws this one of the slot's devices; when it says no, the app's
 * own card is shown instead (a plugin's card for its temperature probes, say,
 * leaving its room sensors the app's). Checked once its module has loaded.
 *
 * A `rule.step` element's class may also have, as statics:
 * - `accepts(value)` — whether it can edit that step; when it says no, the
 *   editor's generic form is shown instead;
 * - `describe(host, deviceIds, value)` — the step in words, for the
 *   automation's sentence, with `{devices}` where the devices' names go
 *   ("apply “Halloween 1” on {devices}"); the app fills them in.
 *
 * Added after the first release of version 1 (app 0.1.18): `fetch`,
 * `upload`, `store`, `Device.sw_version` and the `rule.step` slot. Check for
 * them (`typeof host.upload === 'function'`) to run on an older app. Later
 * (app 0.1.21): `units` — without it, show the schema's own units. Later
 * still (the app after 0.1.25): a `device.card` class's `accepts` — an older app shows
 * the plugin's card for every device of the slot's types.
 * Later (the app after 0.1.27): `fileUrl` — without it, draw no picture the
 * plugin keeps.
 * Later (the app after 0.1.33): `alerts` — without it, decide for yourself
 * (a low battery from `battery_low` and the level).
 *
 * A component runs inside the app, as the person using it: what it can do is
 * what they can. Everything here is scoped to the plugin's own devices and
 * actions all the same, and is the stable way in — the app's own internals
 * are not, and change between releases.
 */

export type Result<T> = { ok: true; data: T } | { ok: false; error: string };

/**
 * One of homeCore's alerts on one of the plugin's devices, as the app's
 * Alerts page shows it: homeCore decided it (its low-battery line, its
 * alert rules, what someone ignored), so a screen that shows it agrees.
 */
export interface Alert {
	/** Stable while it lasts: `low_battery:{device}`, `rule:{rule}:{device}`. */
	id: string;
	/** `offline`, `wet`, `smoke`, `jammed`, `low_battery`, `rule`, `plugin`, … */
	kind: string;
	device_id: string;
	/** In words, with the device's name: "Hallway Sensor: battery low (14%)". */
	message: string;
	/** The same without the name, for the device's own screen: "Battery low (14%)". */
	reason: string;
	severity: 'info' | 'warning' | 'error' | 'critical';
	raised_at: string;
	/** Someone has seen it; it stays until it clears. */
	acknowledged: boolean;
}

/** One of the plugin's devices, as homeCore reports it. */
export interface Device {
	device_id: string;
	/** The plugin's name for it. */
	name: string;
	/** What the house calls it, when someone renamed it. */
	name_override?: string | null;
	device_type?: string | null;
	plugin_id: string;
	available: boolean;
	/** Its room's name, if it is in one. */
	room: string | null;
	attributes: Record<string, unknown>;
	last_seen?: string | null;
	/** Its firmware or software version, as the plugin reports it. */
	sw_version?: string | null;
}

export interface Question {
	title: string;
	body?: string;
	/** What is lost, one line each. */
	lost?: string[];
	/** The button that goes ahead — a verb. */
	go: string;
	stay?: string;
	danger?: boolean;
}

export interface Host {
	/** The interface version: 1. */
	readonly api: 1;
	readonly pluginId: string;
	/** The app's version, for a component that wants to say so. */
	readonly appVersion: string;

	/** Who is using it. Decide what to offer with `can`, never the role. */
	readonly me: {
		readonly name: string;
		readonly role: string;
		readonly isAdmin: boolean;
		can(scope: string): boolean;
	};

	/** The plugin's devices, live. */
	readonly devices: {
		list(): Device[];
		get(id: string): Device | undefined;
		/** Called now, and again on every change; returns the way to stop. */
		subscribe(f: (devices: Device[]) => void): () => void;
		/** Shown as the house shows it: the name someone gave it, else the plugin's. */
		label(d: Device): string;
		/** One attribute's past values, oldest first. */
		history(
			id: string,
			attribute: string,
			opts?: { from?: string; limit?: number }
		): Promise<Result<{ at: string; value: unknown }[]>>;
	};

	/**
	 * homeCore's alerts on the plugin's devices, live — what the Alerts page
	 * lists. Show these instead of deciding again ("is this battery low?").
	 * Absent on an app before 0.1.34: decide for yourself there.
	 */
	readonly alerts?: {
		/** `undefined` until homeCore has said (or on one too old to have alerts). */
		list(): Alert[] | undefined;
		/** Called now, and again on every change; returns the way to stop. */
		subscribe(f: (alerts: Alert[] | undefined) => void): () => void;
	};

	/** A command to one of its devices, in the device's own words. */
	command(deviceId: string, body: Record<string, unknown>): Promise<Result<void>>;
	/** One of the plugin's manifest actions; its declared role applies. */
	action(id: string, params?: Record<string, unknown>): Promise<Result<Record<string, unknown>>>;
	/** A read-only manifest action — for anyone who can see devices. */
	query(id: string, params?: Record<string, unknown>): Promise<Result<Record<string, unknown>>>;

	/**
	 * homeCore's API as the person using it: `path` is under `/api/v1`
	 * (`/automations`). For what the rest of `host` doesn't cover.
	 */
	fetch(path: string, init?: RequestInit): Promise<Response>;

	/**
	 * Where a picture the plugin keeps while it runs is — a channel's icon,
	 * say: `path` in its files folder (`icons/837-2.23.png`). For an `<img
	 * src>`; the browser keeps it for good, as the plugin names a file by what
	 * is in it. Absent before the app after 0.1.27.
	 */
	fileUrl?(path: string): string;

	/**
	 * One of the plugin's manifest actions, sent a file (up to 32 MiB) — the
	 * plugin gets a local `file_path`. `params` arrive as strings.
	 */
	upload(
		action: string,
		file: Blob,
		params?: Record<string, string>,
		name?: string
	): Promise<Result<Record<string, unknown>>>;

	/**
	 * Documents the app's server keeps for the plugin's screens — a copy of a
	 * catalogue, a remembered choice; up to 2 MiB each. Shared by everyone in
	 * the house who can see devices. Keys: letters, digits, `-` and `_`.
	 */
	readonly store: {
		get<T = unknown>(key: string): Promise<Result<T | undefined>>;
		put(key: string, value: unknown): Promise<Result<void>>;
		remove(key: string): Promise<Result<void>>;
	};

	/** The plugin's own settings file (admins). */
	readonly config: {
		get(): Promise<Result<Record<string, unknown>>>;
		/** Write some fields of one section, in place; restart the plugin to take them. */
		patch(section: string, values: Record<string, unknown>): Promise<Result<void>>;
		restartPlugin(): Promise<Result<void>>;
	};

	/**
	 * The units the person reads in, as they chose them in the app: °F or °C,
	 * and US (inHg, mph, in, in/h) or metric (hPa, m/s, mm, mm/h) for the
	 * rest. homeCore's readings are °C and metric (the schema's `unit`); show
	 * them with `show`, and turn a number the person typed back with `toCore`.
	 * Absent on an app before 0.1.21 — show the schema's unit there.
	 */
	readonly units?: Units;

	/** The app's own ways of asking and going. */
	readonly ui: {
		confirm(q: Question): Promise<boolean>;
		openDevice(id: string): void;
		navigate(href: string): void;
	};

	/**
	 * The app's look. Its colours and type are CSS custom properties, which
	 * reach into a shadow root; `sheet` adds the app's parts as classes —
	 * adopt it: `root.adoptedStyleSheets = [host.theme.sheet, ...]`.
	 *
	 * Classes: `hc-card`, `hc-head`, `hc-label`, `hc-num`, `hc-quiet`,
	 * `hc-pill` (+ `go`, `danger`), `hc-field`, `hc-row`, `hc-grid`,
	 * `hc-table`, `hc-tag` (+ `good`, `warn`, `bad`), `hc-dot` (+ the same);
	 * for a card, `hc-tile` (+ `compact`, `lit`) with `hc-tile-name` and
	 * `hc-tile-state` — the app's own card frame.
	 */
	readonly theme: {
		readonly sheet: CSSStyleSheet;
		dark(): boolean;
	};
}

/** The person's units (`host.units`). */
export interface Units {
	readonly temperature: 'F' | 'C';
	readonly measures: 'us' | 'metric';
	/**
	 * A reading in `unit` — the schema's: `°C`, `hPa`, `m/s`, `mm`, `mm/h`,
	 * `km` — as the person reads it: `{ value: 70.7, unit: '°F', text:
	 * '70.7°F' }` (degrees and % against the number, other units after a
	 * space), rounded as that unit reads. Any other unit (`%`, `lux`)
	 * comes back as it was, the value untouched.
	 */
	show(
		value: number,
		unit: string | undefined
	): { value: number; unit: string | undefined; text: string };
	/** A number the person gave in their unit, back in `unit`: 70 (°F) → 21.111 for `°C`. */
	toCore(value: number, unit: string | undefined): number;
	/** Called now, and again whenever the person changes them; returns the way to stop. */
	subscribe(f: (units: { temperature: 'F' | 'C'; measures: 'us' | 'metric' }) => void): () => void;
}

/** The properties the app sets on a plugin's element. */
export interface PluginElement extends HTMLElement {
	host: Host;
	/** On a `device.panel` element. */
	deviceId?: string;
	/** On a `rule.step` element. */
	deviceIds?: string[];
	value?: Record<string, unknown>;
	/** On a card element. */
	form?: 'compact' | 'standard' | 'large';
	/** On a `dashboard.card` element, and its settings element. */
	config?: Record<string, unknown>;
}

/** What a `device.card` element's class may say about a device. */
export interface CardElementClass {
	accepts?(device: Device): boolean;
}

/** What a `rule.step` element's class may offer the editor. */
export interface StepElementClass {
	accepts?(value: Record<string, unknown>): boolean;
	describe?(host: Host, deviceIds: string[], value: Record<string, unknown>): string | undefined;
}
