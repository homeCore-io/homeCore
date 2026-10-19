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
 * - `deviceId` — on a `device.panel` element: the device being shown.
 *
 * A component runs inside the app, as the person using it: what it can do is
 * what they can. Everything here is scoped to the plugin's own devices and
 * actions all the same, and is the stable way in — the app's own internals
 * are not, and change between releases.
 */

export type Result<T> = { ok: true; data: T } | { ok: false; error: string };

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

	/** A command to one of its devices, in the device's own words. */
	command(deviceId: string, body: Record<string, unknown>): Promise<Result<void>>;
	/** One of the plugin's manifest actions; its declared role applies. */
	action(id: string, params?: Record<string, unknown>): Promise<Result<Record<string, unknown>>>;
	/** A read-only manifest action — for anyone who can see devices. */
	query(id: string, params?: Record<string, unknown>): Promise<Result<Record<string, unknown>>>;

	/** The plugin's own settings file (admins). */
	readonly config: {
		get(): Promise<Result<Record<string, unknown>>>;
		/** Write some fields of one section, in place; restart the plugin to take them. */
		patch(section: string, values: Record<string, unknown>): Promise<Result<void>>;
		restartPlugin(): Promise<Result<void>>;
	};

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
	 * `hc-table`, `hc-tag` (+ `good`, `warn`, `bad`), `hc-dot` (+ the same).
	 */
	readonly theme: {
		readonly sheet: CSSStyleSheet;
		dark(): boolean;
	};
}

/** The properties the app sets on a plugin's element. */
export interface PluginElement extends HTMLElement {
	host: Host;
	/** On a `device.panel` element. */
	deviceId?: string;
}
