/**
 * A WLED controller's catalogue — each effect's controls, each palette's
 * colours — for the screens that need it, fetched once per controller and
 * firmware while the app is open.
 *
 * Where it comes from is the controller's own setting (`catalog_storage`):
 * read from the controller (hc-wled's read-only `get_catalog`, through
 * homeCore), or a copy kept in the app's store for these screens
 * (`host.store`), which works while the controller is offline — taken again
 * when its firmware changes.
 */
import type { CoreDevice } from "./devices.js";
import { host } from "./host.js";
import { catalogStorage, type Catalog } from "./wled.js";

type Kept = { firmware: string; catalog: Catalog };
const keyOf = (id: string) => `catalog-${id.replace(/[^A-Za-z0-9_-]/g, "_")}`;

/** Asked of the controller, through homeCore. */
async function ask(id: string): Promise<Catalog> {
  const r = await host().query("get_catalog", { device_id: id });
  if (!r.ok) throw new Error(r.error);
  const {
    status,
    error,
    request_id: _r,
    ...catalog
  } = r.data as Catalog & {
    status?: string;
    error?: string;
    request_id?: string;
  };
  if (status === "error") throw new Error(error ?? "the controller said no");
  return catalog as Catalog;
}

/** The copy kept in the app: used when it is from this firmware; taken again when not. */
async function kept(
  id: string,
  firmware: string,
  refresh: boolean,
): Promise<Catalog> {
  const got = await host().store.get<Kept>(keyOf(id));
  const copy = got.ok ? got.data : undefined;
  if (copy && !refresh && (firmware === "" || copy.firmware === firmware))
    return copy.catalog;
  try {
    const catalog = await ask(id);
    void host().store.put(keyOf(id), { firmware, catalog } satisfies Kept);
    return catalog;
  } catch (e) {
    // Offline: the copy from another firmware beats nothing.
    if (copy) return copy.catalog;
    throw e;
  }
}

interface Entry {
  catalog?: Catalog;
  loading: boolean;
  error?: string;
  /** The firmware it was read from: a new one means new effects. */
  firmware: string;
}

const firmwareOf = (d: CoreDevice) => String(d.sw_version ?? "");

class WledCatalogs {
  byId = $state<Record<string, Entry>>({});

  /** The catalogue for `d` as far as it has loaded. Read it in markup;
   * call `ensure` from an effect to have it loaded. */
  of(d: CoreDevice): Entry {
    return this.byId[d.device_id] ?? { loading: true, firmware: firmwareOf(d) };
  }

  /** Load it, unless it is loaded (from this firmware) or loading. */
  ensure(d: CoreDevice): void {
    const firmware = firmwareOf(d);
    const e = this.byId[d.device_id];
    // A failure stays until someone asks again (`load`), or the loop of
    // effect → load → state → effect would never end.
    if (e && (e.loading || (e.firmware === firmware && (e.catalog || e.error))))
      return;
    void this.load(d, firmware);
  }

  async load(d: CoreDevice, firmware: string, refresh = false): Promise<void> {
    const id = d.device_id;
    this.byId = {
      ...this.byId,
      [id]: { ...this.byId[id], loading: true, firmware },
    };
    let entry: Entry;
    try {
      const catalog =
        catalogStorage(d) === "app"
          ? await kept(id, firmware, refresh)
          : await ask(id);
      entry = { catalog, loading: false, firmware };
    } catch (e) {
      entry = { loading: false, firmware, error: (e as Error).message };
    }
    this.byId = { ...this.byId, [id]: entry };
  }
}

export const wledCatalogs = new WledCatalogs();
