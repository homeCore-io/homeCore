/** Who is using the screens, through `host`. */
import { host } from "./host.js";
export const session = {
  get isAdmin(): boolean {
    return host().me.isAdmin;
  },
  can: (scope: string): boolean => host().me.can(scope),
};
