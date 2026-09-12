import { Sha256 } from "@aws-crypto/sha256-js";
import { writable } from "svelte/store";
import type { ServerConfig } from "./serverSync";
import { parseServerRegistration } from "./serverSyncRegistration";

/** One transient owner. Navigation sees only a revision, never a URI or credential. */
export function createRegistrationInbox() {
  let pending: ServerConfig | undefined;
  let revision = 0;
  let fingerprint: string | undefined;
  const changed = writable(0);
  return {
    changed: { subscribe: changed.subscribe },
    stage(uri: string, notify = true): boolean {
      const parsed = parseServerRegistration(uri);
      const hash = new Sha256();
      hash.update(uri);
      const next = Array.from(hash.digestSync(), (byte) =>
        byte.toString(16).padStart(2, "0"),
      ).join("");
      if (next === fingerprint) return false;
      fingerprint = next;
      pending = parsed;
      if (notify) changed.set(++revision);
      return true;
    },
    flush(): void {
      changed.set(++revision);
    },
    take(): ServerConfig | undefined {
      const value = pending;
      pending = undefined;
      return value;
    },
    clear(): void {
      pending = undefined;
      fingerprint = undefined;
    },
  };
}
export const serverRegistrationInbox = createRegistrationInbox();
