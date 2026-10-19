/**
 * What every WLED element does first: hand the adapters the `host`, and put
 * the app's look and WLED's own form styles in its shadow root.
 */
import type { Host } from "#lib/host.js";
import { setHost } from "#lib/host.js";
import formCss from "#lib/components/wled/wledForm.css?inline";

let form: CSSStyleSheet | undefined;

export function start(el: HTMLElement | undefined, h: Host | undefined): void {
  if (!h) return;
  setHost(h);
  const root = el?.shadowRoot;
  if (!root) return;
  if (!form) {
    form = new CSSStyleSheet();
    form.replaceSync(formCss);
  }
  const want = [h.theme.sheet, form];
  if (!want.every((s) => root.adoptedStyleSheets.includes(s)))
    root.adoptedStyleSheets = [
      ...want,
      ...root.adoptedStyleSheets.filter((s) => !want.includes(s)),
    ];
}
