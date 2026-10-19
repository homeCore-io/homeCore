/** A rule as homeCore sends it — only what these screens read. */
export interface Rule {
  id: string;
  name: string;
  enabled: boolean;
  priority?: number;
  tags?: string[];
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  trigger: any;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  conditions?: any[];
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  actions?: { enabled?: boolean; action: any }[];
  error?: string;
}
