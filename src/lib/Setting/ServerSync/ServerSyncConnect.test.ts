// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import { mount, tick, unmount } from "svelte";
vi.mock("src/ts/platform", () => ({ isTauriAndroid: false }));
vi.mock("src/lang", async () => ({
  language: (await import("src/lang/en")).languageEnglish,
}));
import { languageEnglish } from "src/lang/en";
import { serverRegistrationInbox } from "src/ts/storage/sync/serverSyncRegistrationInbox";
import vector from "../../../../crates/sync-connect/tests/registration-vector.json";
import ServerSyncConnect from "./ServerSyncConnect.svelte";

const text = languageEnglish.risuNest.serverSync;
let component: ReturnType<typeof mount> | undefined;
let target: HTMLDivElement;
afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  serverRegistrationInbox.clear();
  document.body.replaceChildren();
});
function setup(props: Record<string, unknown> = {}) {
  target = document.createElement("div");
  document.body.append(target);
  const onSubmit = vi.fn();
  component = mount(ServerSyncConnect, { target, props: { onSubmit, ...props } });
  return onSubmit;
}
const button = (label: string) =>
  [...target.querySelectorAll<HTMLButtonElement>("button")].find((b) =>
    b.textContent?.trim().startsWith(label),
  )!;

describe("shared connect part", () => {
  it("shows the check for a read code and reports the chosen asset policy with it", async () => {
    const onSubmit = setup();
    await tick();
    const input = target.querySelector("input")!;
    input.value = vector.uri;
    input.dispatchEvent(new Event("input", { bubbles: true }));
    await tick();
    button(text.readRegistration).click();
    await tick();
    const review = target.querySelector("dl.review")!;
    expect(review.textContent).toContain("https://sync.example/base/");
    expect(review.textContent).toContain(vector.registration.directory.baseUrl);
    expect(target.textContent).not.toContain(vector.registration.token);
    expect(target.querySelector("form.fields")).toBeNull();
    const remote = target.querySelectorAll<HTMLButtonElement>('[role="radio"]')[1];
    remote.click();
    await tick();
    expect(remote.getAttribute("aria-checked")).toBe("true");
    button(text.connect).click();
    await tick();
    expect(onSubmit).toHaveBeenCalledExactlyOnceWith({
      config: { ...vector.registration, endpoint: "https://sync.example/base/" },
      residency: "remote",
    });
  });
  it("turns a manual entry into the same check and defaults to keeping files on this device", async () => {
    const onSubmit = setup();
    await tick();
    const inputs = [...target.querySelectorAll<HTMLInputElement>("form.fields input")];
    ["https://example.test", "lib", "device", "c".repeat(64)].forEach((value, index) => {
      inputs[index].value = value;
      inputs[index].dispatchEvent(new Event("input", { bubbles: true }));
    });
    target
      .querySelector("form.fields")!
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await tick();
    expect(target.querySelector("dl.review")!.textContent).toContain("https://example.test/");
    expect(onSubmit).not.toHaveBeenCalled();
    button(text.connect).click();
    expect(onSubmit).toHaveBeenCalledExactlyOnceWith({
      config: {
        endpoint: "https://example.test/",
        libraryId: "lib",
        deviceId: "device",
        token: "c".repeat(64),
      },
      residency: "full",
    });
  });
  it("rejects a bad manual address in place and never reaches the check", async () => {
    const onSubmit = setup();
    await tick();
    const inputs = [...target.querySelectorAll<HTMLInputElement>("form.fields input")];
    ["http://example.test", "lib", "device", "c".repeat(64)].forEach((value, index) => {
      inputs[index].value = value;
      inputs[index].dispatchEvent(new Event("input", { bubbles: true }));
    });
    target
      .querySelector("form.fields")!
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await tick();
    expect(target.querySelector("dl.review")).toBeNull();
    expect(target.querySelector('[role="alert"]')!.textContent).toContain("(https-required)");
    expect(onSubmit).not.toHaveBeenCalled();
  });
  it("marks the replacement and names its button after re-registration", async () => {
    const onSubmit = setup({
      replacing: true,
      initialNavigation: { endpoint: "https://bound.test/", libraryId: "bound" },
    });
    await tick();
    const inputs = [...target.querySelectorAll<HTMLInputElement>("form.fields input")];
    expect(inputs.map((input) => input.value)).toEqual(["https://bound.test/", "bound", "", ""]);
    inputs[2].value = "device2";
    inputs[2].dispatchEvent(new Event("input", { bubbles: true }));
    inputs[3].value = "d".repeat(64);
    inputs[3].dispatchEvent(new Event("input", { bubbles: true }));
    target
      .querySelector("form.fields")!
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await tick();
    button(text.reregister).click();
    expect(onSubmit).toHaveBeenCalledExactlyOnceWith(
      expect.objectContaining({ replacing: true, residency: "full" }),
    );
  });
});
