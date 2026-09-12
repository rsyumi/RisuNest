import { expect, it, vi } from "vitest";
import {
  notifyLocalPersistentRevision,
  subscribeLocalPersistentRevision,
} from "./persistentRevisionEvents";

it("isolates failed revision observers from completed saves and other observers", () => {
  const received = vi.fn();
  const stopBroken = subscribeLocalPersistentRevision(() => {
    throw new Error("observer failed");
  });
  const stop = subscribeLocalPersistentRevision(received);
  expect(() => notifyLocalPersistentRevision(17)).not.toThrow();
  expect(received).toHaveBeenCalledWith(17);
  stop();
  notifyLocalPersistentRevision(18);
  expect(received).toHaveBeenCalledTimes(1);
  stopBroken();
});
