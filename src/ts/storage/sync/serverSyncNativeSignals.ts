/** Native change notifications for the synchronisation scheduler. They carry no
 * payload: a notification only says that something may have moved, and the run
 * it brings forward confirms the remote head. */
export const SERVER_SYNC_DEVICE_CHANGED_EVENT = "risu-server-sync-device-changed";
export const SERVER_SYNC_REMOTE_HINT_EVENT = "risu-server-sync-remote-hint";

export interface ServerSyncSignalTarget {
  deviceChanged(): void;
  remoteHint(): void;
}
export type NativeEventSubscriber = (
  event: string,
  handler: () => void,
) => Promise<() => void>;

export function subscribeNativeServerSyncSignals(
  target: ServerSyncSignalTarget,
  listen: NativeEventSubscriber,
): () => void {
  let disposed = false;
  const disposers: (() => void)[] = [];
  const attach = (event: string, handler: () => void) => {
    listen(event, handler).then(
      (dispose) => {
        if (disposed) dispose();
        else disposers.push(dispose);
      },
      // A subscription that never arrives costs latency, not convergence.
      () => {},
    );
  };
  attach(SERVER_SYNC_DEVICE_CHANGED_EVENT, () => target.deviceChanged());
  attach(SERVER_SYNC_REMOTE_HINT_EVENT, () => target.remoteHint());
  return () => {
    disposed = true;
    for (const dispose of disposers.splice(0)) dispose();
  };
}
