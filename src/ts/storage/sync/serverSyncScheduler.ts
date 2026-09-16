import type {
  ServerSyncController,
  ServerSyncSnapshot,
} from "./serverSyncController";

/** Schedules durable local revisions, never streamed tokens. Transport remains
 * single-flight in the controller, and a revision arriving during a run survives
 * as another scheduled check. Foreground and network state are explicit inputs. */
export function createServerSyncScheduler(
  controller: ServerSyncController,
  options: { available(): boolean; random?: () => number },
) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let localSince: number | undefined;
  let localDue: number | undefined;
  let hintDue: number | undefined;
  let failures = 0;
  let poll = 60_000;
  let previousHead: string | undefined;
  let previous = { ...controller.snapshot() };
  let automatic = false;
  let stopped = false;
  const random = options.random ?? Math.random;
  const clear = () => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  };
  const schedule = (delay: number) => {
    clear();
    if (stopped || !options.available() || !controller.canAutoSync()) return;
    timer = setTimeout(run, delay);
  };
  const due = () => {
    const pending = [localDue, hintDue].filter(
      (value): value is number => value !== undefined,
    );
    return pending.length ? Math.min(...pending) : undefined;
  };
  const run = () => {
    clear();
    if (stopped || !options.available() || !controller.canAutoSync()) return;
    localSince = localDue = hintDue = undefined;
    automatic = true;
    void controller.synchronize();
    automatic = false;
  };
  const completed = (state: ServerSyncSnapshot) => {
    if (state.error) {
      failures += 1;
      const ceiling = Math.min(60_000, 1000 * 2 ** Math.min(failures - 1, 6));
      schedule(Math.min(60_000, Math.round(ceiling * (0.8 + random() * 0.4))));
      return;
    }
    failures = 0;
    const head = state.result?.head?.headId;
    const changed =
      head !== previousHead ||
      Boolean(state.result?.appliedRecords || state.result?.proposedRecords);
    previousHead = head;
    poll = changed ? 60_000 : Math.min(300_000, poll * 2);
    const next = due();
    if (next !== undefined) schedule(Math.max(0, next - Date.now()));
    else schedule(state.result?.phase === "pending" ? 1000 : poll);
  };
  const unsubscribe = controller.subscribe((state) => {
    const before = previous;
    previous = { ...state };
    if (state.running && !before.running) {
      clear();
      if (!automatic) failures = 0;
    } else if (!state.running && before.running) {
      completed(state);
    } else if (!state.running && !controller.canAutoSync()) {
      clear();
    } else if (
      !state.running &&
      ((!before.status?.configured && state.status?.configured) ||
        (before.paused && !state.paused) ||
        (before.error && !state.error))
    ) {
      failures = 0;
      schedule(0);
    }
  });
  return {
    localCommit() {
      const now = Date.now();
      localSince ??= now;
      localDue = Math.min(now + 500, localSince + 5000);
      // Ongoing edits cannot continuously postpone retries after a failure.
      if (failures === 0) schedule(Math.max(0, (due() ?? localDue) - now));
    },
    /** A notification that the remote may have moved. The run it brings forward
     * confirms the head; a missed notification only costs the poll interval. */
    remoteHint() {
      const now = Date.now();
      if (hintDue !== undefined && hintDue > now) return;
      hintDue = now + 250;
      if (failures === 0) schedule(Math.max(0, (due() ?? hintDue) - now));
    },
    resume() {
      failures = 0;
      poll = 60_000;
      schedule(0);
    },
    suspend() {
      clear();
      if (controller.snapshot().running) void controller.suspend();
    },
    stop() {
      stopped = true;
      clear();
      unsubscribe();
    },
  };
}
