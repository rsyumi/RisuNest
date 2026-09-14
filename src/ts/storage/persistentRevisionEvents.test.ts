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
  expect(received).toHaveBeenCalledWith(17, 'edit');
  stop();
  notifyLocalPersistentRevision(18);
  expect(received).toHaveBeenCalledTimes(1);
  stopBroken();
});

it('reports generation completion only when explicitly identified', () => {
  const received = vi.fn();
  const stop = subscribeLocalPersistentRevision(received);
  notifyLocalPersistentRevision(21);
  notifyLocalPersistentRevision(21, 'generation-complete');
  expect(received).toHaveBeenNthCalledWith(1, 21, 'edit');
  expect(received).toHaveBeenNthCalledWith(2, 21, 'generation-complete');
  stop();
});
