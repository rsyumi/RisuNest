import { afterEach, expect, it, vi } from "vitest";
import { parseRisuLocalUrl } from "./risuLocalUrl";
import { parseServerSyncDeepLink } from "./storage/sync/serverSyncDeepLink";
import { dispatchRisuLocalUrl } from "./deepLinkDispatcher";
import vector from "../../crates/sync-connect/tests/registration-vector.json";
afterEach(() => vi.unstubAllGlobals());
it("does not depend on WebView custom-scheme URL authority support", () => {
  const NativeURL = URL;
  vi.stubGlobal(
    "URL",
    class extends NativeURL {
      constructor(input: string | URL, base?: string | URL) {
        if (String(input).startsWith("risunestlocal:"))
          throw new Error("opaque custom scheme");
        super(input, base);
      }
    },
  );
  expect(
    parseServerSyncDeepLink(
      "risunestlocal://sync-server/connect?libraryId=library",
    ),
  ).toEqual({ libraryId: "library" });
  const registration = vi.fn();
  expect(
    dispatchRisuLocalUrl(vector.uri, {
      onRealm: vi.fn(),
      onServerSync: vi.fn(),
      onServerRegistration: registration,
    }),
  ).toBe(true);
  expect(registration).toHaveBeenCalledExactlyOnceWith(vector.uri);
  const realm = vi.fn();
  expect(
    dispatchRisuLocalUrl("risunestlocal://realm/synthetic-id", {
      onRealm: realm,
      onServerSync: vi.fn(),
    }),
  ).toBe(true);
  expect(realm).toHaveBeenCalledWith("synthetic-id");
});
it("rejects other schemes and keeps navigation credentials forbidden", () => {
  expect(parseRisuLocalUrl("https://sync-server/connect")).toBeNull();
  expect(parseRisuLocalUrl("risunestlocal:sync-server/connect")).toBeNull();
  for (const uri of [
    "risunestlocal://user@sync-server/connect",
    "risunestlocal://sync-server:443/connect",
    "risunestlocal://sync-server\\connect",
    "risunestlocal://sync-server/connect#secret",
    "risunestlocal://sync-server/connect?token=secret",
  ])
    expect(parseServerSyncDeepLink(uri)).toBeNull();
});
