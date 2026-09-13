import {
  cancel,
  checkPermissions,
  requestPermissions,
  scan,
  Format,
} from "@tauri-apps/plugin-barcode-scanner";
import {
  RegistrationError,
  parseServerRegistration,
} from "./serverSyncRegistration";
import type { ServerConfig } from "./serverSync";
export function createServerQrScanner(
  api = { cancel, checkPermissions, requestPermissions, scan },
) {
  let active: { stopped: boolean; stop(): void } | undefined;
  return {
    async scan(onCamera: () => void): Promise<ServerConfig> {
      if (active) throw new RegistrationError("qr-scan-busy");
      let stop!: () => void;
      const stopped = new Promise<never>((_, reject) => {
        stop = () => reject(new RegistrationError("qr-scan-cancelled"));
      });
      const session = { stopped: false, stop };
      active = session;
      let timer: ReturnType<typeof setTimeout> | undefined;
      const work = async () => {
        let permission: string = await api.checkPermissions();
        if (session.stopped) throw new RegistrationError("qr-scan-cancelled");
        if (permission === "prompt" || permission === "prompt-with-rationale")
          permission = await api.requestPermissions();
        if (session.stopped) throw new RegistrationError("qr-scan-cancelled");
        if (permission !== "granted")
          throw new RegistrationError("qr-camera-permission-denied");
        onCamera();
        const result = await api.scan({
          formats: [Format.QRCode],
          windowed: true,
          cameraDirection: "back",
        });
        if (session.stopped) throw new RegistrationError("qr-scan-cancelled");
        if (result.format !== Format.QRCode)
          throw new RegistrationError("invalid-registration");
        return parseServerRegistration(result.content);
      };
      try {
        const timeout = new Promise<never>((_, reject) => {
          timer = setTimeout(
            () => reject(new RegistrationError("qr-scan-timeout")),
            60_000,
          );
        });
        return await Promise.race([work(), stopped, timeout]);
      } catch (cause) {
        throw cause instanceof RegistrationError
          ? cause
          : new RegistrationError("qr-camera-unavailable");
      } finally {
        session.stopped = true;
        if (timer) clearTimeout(timer);
        await api.cancel().catch(() => undefined);
        if (active === session) active = undefined;
      }
    },
    cancel(): void {
      if (active) {
        active.stopped = true;
        active.stop();
      }
    },
  };
}
