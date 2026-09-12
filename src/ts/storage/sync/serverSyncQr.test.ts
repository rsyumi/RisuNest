import { describe, expect, it, vi } from "vitest";
import { createServerQrScanner } from "./serverSyncQr";
import { createRegistrationInbox } from "./serverSyncRegistrationInbox";
import vector from "../../../../crates/sync-connect/tests/registration-vector.json";
import { Format } from "@tauri-apps/plugin-barcode-scanner";
function api() {
  return {
    cancel: vi.fn().mockResolvedValue(undefined),
    checkPermissions: vi.fn().mockResolvedValue("granted"),
    requestPermissions: vi.fn().mockResolvedValue("granted"),
    scan: vi.fn().mockResolvedValue({
      format: Format.QRCode,
      content: vector.uri,
      bounds: null,
    }),
  };
}
describe("QR registration input", () => {
  it("uses the common parser and closes the scanner on success and invalid input", async () => {
    const driver = api();
    const scanner = createServerQrScanner(driver);
    const ready = vi.fn();
    expect(await scanner.scan(ready)).toEqual(vector.registration);
    expect(ready).toHaveBeenCalledOnce();
    expect(driver.cancel).toHaveBeenCalledOnce();
    driver.scan.mockResolvedValue({
      format: Format.QRCode,
      content: "secret invalid input",
      bounds: null,
    });
    await expect(scanner.scan(ready)).rejects.toThrow(
      "invalid-registration-uri",
    );
    expect(driver.cancel).toHaveBeenCalledTimes(2);
  });
  it("does not start the camera when permission is denied", async () => {
    const driver = api();
    driver.checkPermissions.mockResolvedValue("denied");
    await expect(createServerQrScanner(driver).scan(vi.fn())).rejects.toThrow(
      "qr-camera-permission-denied",
    );
    expect(driver.scan).not.toHaveBeenCalled();
  });
  it("cancels during permission request without subsequently opening a camera", async () => {
    const driver = api();
    let grant!: (value: string) => void;
    driver.checkPermissions.mockReturnValue(
      new Promise((resolve) => {
        grant = resolve;
      }),
    );
    const scanner = createServerQrScanner(driver);
    const pending = scanner.scan(vi.fn());
    scanner.cancel();
    await expect(pending).rejects.toThrow("qr-scan-cancelled");
    grant("granted");
    await Promise.resolve();
    expect(driver.scan).not.toHaveBeenCalled();
  });
  it("single-flights scanning and resolves cancellation even if the native scan stays pending", async () => {
    const driver = api();
    driver.scan.mockReturnValue(new Promise(() => {}));
    const scanner = createServerQrScanner(driver);
    const pending = scanner.scan(vi.fn());
    await expect(scanner.scan(vi.fn())).rejects.toThrow("qr-scan-busy");
    scanner.cancel();
    await expect(pending).rejects.toThrow("qr-scan-cancelled");
    expect(driver.cancel).toHaveBeenCalledOnce();
  });
  it("keeps OS-delivered secrets transient and consumed only once", () => {
    const inbox = createRegistrationInbox();
    inbox.stage(vector.uri);
    expect(inbox.take()).toEqual(vector.registration);
    expect(inbox.take()).toBeUndefined();
    expect(inbox.stage(vector.uri)).toBe(false);
    expect(inbox.take()).toBeUndefined();
    inbox.clear();
    expect(inbox.stage(vector.uri)).toBe(true);
    inbox.clear();
    expect(inbox.take()).toBeUndefined();
  });
});
