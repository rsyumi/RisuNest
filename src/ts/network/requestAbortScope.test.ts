import { expect, it, vi } from "vitest";
import { createRequestAbortScope } from "./requestAbortScope";

it("retains SDK cancellation without aborting the caller and removes both listeners", async () => {
  const caller = new AbortController();
  const sdk = new AbortController();
  const callerRemove = vi.spyOn(caller.signal, "removeEventListener");
  const sdkRemove = vi.spyOn(sdk.signal, "removeEventListener");
  let received!: AbortSignal;
  const scope = createRequestAbortScope(caller.signal);
  await scope.fetch(async (_input, init) => {
    received = init!.signal!;
    return new Response(null);
  })("https://synthetic.invalid", { signal: sdk.signal });
  sdk.abort("sdk cancelled");
  expect(received.aborted).toBe(true);
  expect(received.reason).toBe("sdk cancelled");
  expect(caller.signal.aborted).toBe(false);
  scope.dispose();
  expect(callerRemove).toHaveBeenCalled();
  expect(sdkRemove).toHaveBeenCalled();
  const removed = [callerRemove.mock.calls.length, sdkRemove.mock.calls.length];
  scope.dispose();
  expect([callerRemove.mock.calls.length, sdkRemove.mock.calls.length]).toEqual(
    removed,
  );
});

it("passes an already aborted caller signal to a request", async () => {
  const caller = new AbortController();
  caller.abort("expired");
  const scope = createRequestAbortScope(caller.signal);
  await scope.fetch(async (_input, init) => {
    expect(init!.signal!.aborted).toBe(true);
    expect(init!.signal!.reason).toBe("expired");
    return new Response(null);
  })("https://synthetic.invalid");
  scope.dispose();
});
