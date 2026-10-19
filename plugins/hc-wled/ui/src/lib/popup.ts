/** Opening a device's panel, through `host`. */
import { host } from "./host.js";

/** A link to a device's panel over the page as it is. */
export function deviceHref(id: string, from: URL): string {
  const url = new URL(from.href);
  url.searchParams.set("device", id);
  return url.pathname + url.search;
}
export const openDevice = (id: string): void => host().ui.openDevice(id);
