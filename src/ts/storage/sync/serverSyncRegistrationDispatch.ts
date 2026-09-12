import { tick } from "svelte";
import { serverRegistrationInbox } from "./serverSyncRegistrationInbox";

/** Open public navigation first; deliver the credential after the destination settles. */
export async function receiveServerRegistration(
  uri: string,
  navigate: () => void,
  inbox = serverRegistrationInbox,
): Promise<boolean> {
  try {
    // Suppress duplicate OS events before navigation can clear the current form.
    if (!inbox.stage(uri, false)) return false;
    navigate();
    await tick();
    inbox.flush();
    return true;
  } catch {
    inbox.clear();
    return false;
  }
}
