// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import { mount, tick, unmount } from "svelte";
vi.mock("src/ts/platform", () => ({ isTauriAndroid: false }));
import Input from "./ServerSyncRegistrationInput.svelte";
import { serverRegistrationInbox } from "src/ts/storage/sync/serverSyncRegistrationInbox";
import vector from "../../../../crates/sync-connect/tests/registration-vector.json";
let component: ReturnType<typeof mount> | undefined;
afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  serverRegistrationInbox.clear();
  document.body.replaceChildren();
});
describe("registration form input", () => {
  it("prefills only through explicit Read code and clears the raw secret", async () => {
    const target = document.createElement("div");
    document.body.append(target);
    const onRegistration = vi.fn();
    component = mount(Input, { target, props: { onRegistration } });
    await tick();
    const input = target.querySelector("input")!;
    input.value = vector.uri;
    input.dispatchEvent(new Event("input", { bubbles: true }));
    await tick();
    expect(onRegistration).not.toHaveBeenCalled();
    target.querySelector("button")!.click();
    await tick();
    expect(onRegistration).toHaveBeenCalledWith(vector.registration);
    expect(input.value).toBe("");
    expect(target.textContent).not.toContain(vector.registration.token);
  });
  it("consumes OS delivery once and refuses replacement of an existing connection", async () => {
    serverRegistrationInbox.stage(vector.uri);
    const target = document.createElement("div");
    document.body.append(target);
    const onRegistration = vi.fn();
    component = mount(Input, {
      target,
      props: { available: false, onRegistration },
    });
    await tick();
    expect(onRegistration).not.toHaveBeenCalled();
    await vi.waitFor(() =>
      expect(target.querySelector("[role=status]")).not.toBeNull(),
    );
    expect(serverRegistrationInbox.take()).toBeUndefined();
  });
});
