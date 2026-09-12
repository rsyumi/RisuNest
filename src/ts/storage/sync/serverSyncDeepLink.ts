import { parseRisuLocalUrl } from "../../risuLocalUrl";
import { writable } from "svelte/store";
import {
  validateServerSyncEndpoint,
  validateServerSyncId,
} from "./serverSyncConnection";

export interface ServerSyncNavigation {
  endpoint?: string;
  libraryId?: string;
}

/** A navigation request contains no credential and never binds a controller. */
export function parseServerSyncDeepLink(
  value: string,
): ServerSyncNavigation | null {
  try {
    const url = parseRisuLocalUrl(value);
    if (
      !url ||
      url.hostname !== "sync-server" ||
      url.pathname !== "/connect" ||
      url.username ||
      url.password ||
      url.port ||
      value.includes("#") ||
      value
        .slice(value.indexOf(":") + 3)
        .split(/[/?#]/, 1)[0]
        .includes("@")
    )
      return null;
    const result: ServerSyncNavigation = {};
    const seen = new Set<string>();
    for (const [key, input] of url.searchParams) {
      if (seen.has(key)) return null;
      seen.add(key);
      if (key === "endpoint")
        result.endpoint = validateServerSyncEndpoint(input);
      else if (key === "libraryId")
        result.libraryId = validateServerSyncId(input);
      else return null;
    }
    return result;
  } catch {
    return null;
  }
}

export function createServerSyncNavigationQueue() {
  let pending: ServerSyncNavigation | undefined;
  let receiver: ((request: ServerSyncNavigation) => void) | undefined;
  return {
    receive(uri: string): boolean {
      const request = parseServerSyncDeepLink(uri);
      if (!request) return false;
      if (receiver) receiver(request);
      else pending = request;
      return true;
    },
    subscribe(listener: (request: ServerSyncNavigation) => void): () => void {
      receiver = listener;
      if (pending) {
        const request = pending;
        pending = undefined;
        listener(request);
      }
      return () => {
        if (receiver === listener) receiver = undefined;
      };
    },
  };
}

export const serverSyncNavigation = createServerSyncNavigationQueue();

// Public values only. A5 secrets belong to its separate transient input owner.
export const serverSyncScreenRequest = writable<
  ServerSyncNavigation | undefined
>(undefined);
