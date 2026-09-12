import { expect, it, vi } from "vitest";
import { createRegistrationInbox } from "./serverSyncRegistrationInbox";
import { receiveServerRegistration } from "./serverSyncRegistrationDispatch";
import vector from "../../../../crates/sync-connect/tests/registration-vector.json";
it("navigates without secrets before delivering and suppresses duplicate navigation", async () => {
  const inbox = createRegistrationInbox();
  const order: string[] = [];
  const unsubscribe = inbox.changed.subscribe(() => {
    if (inbox.take()) order.push("credential");
  });
  const navigate = vi.fn(() => order.push("navigate"));
  const first = receiveServerRegistration(vector.uri, navigate, inbox);
  const duplicate = receiveServerRegistration(vector.uri, navigate, inbox);
  expect(await first).toBe(true);
  expect(await duplicate).toBe(false);
  expect(order).toEqual(["navigate", "credential"]);
  expect(navigate).toHaveBeenCalledWith();
  unsubscribe();
});
it("invalid input never opens navigation or survives in the inbox", async () => {
  const inbox = createRegistrationInbox();
  const navigate = vi.fn();
  expect(
    await receiveServerRegistration("invalid private code", navigate, inbox),
  ).toBe(false);
  expect(navigate).not.toHaveBeenCalled();
  expect(inbox.take()).toBeUndefined();
});

it("holds a pending event across outgoing-view cleanup until navigation has settled", async () => {
  const inbox = createRegistrationInbox();
  let received: unknown;
  let unsubscribe = () => {};
  await receiveServerRegistration(
    vector.uri,
    () => {
      inbox.releaseConsumed();
      unsubscribe = inbox.changed.subscribe(() => {
        received = inbox.take() ?? received;
      });
      expect(received).toBeUndefined();
    },
    inbox,
  );
  expect(received).toEqual(vector.registration);
  unsubscribe();
});
