/** The app's own "are you sure?", through `host`. */
import type { Question } from "../../../../../sdk/ui/index.js";
import { host } from "./host.js";
export type { Question };
export const ask = (q: Question): Promise<boolean> => host().ui.confirm(q);
