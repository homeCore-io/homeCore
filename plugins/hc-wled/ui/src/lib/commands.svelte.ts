/** Commands to the controllers, through `host`. */
import { host } from "./host.js";

export const commands = {
  /** One of the device's declared actions. */
  async act(
    id: string,
    action: string,
    params: Record<string, unknown> = {},
  ): Promise<boolean> {
    return (await host().command(id, { action, ...params })).ok;
  },
  /** A command in the device's own words, as it is. */
  async send(id: string, body: Record<string, unknown>): Promise<boolean> {
    return (await host().command(id, body)).ok;
  },
};
