import type { languageEnglish } from "src/lang/en";
import { formatElapsed } from "../../gui/nativeFileJobDialogModel";
import { formatRisuNestStorageBytes } from "../risuNestStorageDashboard";
import type { AssetResidencyPolicy } from "./serverAssetResidency";
import type { ServerConfig, ServerSyncProgress } from "./serverSync";
import type { ServerSyncSnapshot } from "./serverSyncController";

/** The strings both connection screens draw from. */
export type ServerSyncText = (typeof languageEnglish)["risuNest"]["serverSync"];

export interface ServerSyncConnectRequest {
  config: ServerConfig;
  residency: AssetResidencyPolicy;
  /** Replace this library's device credentials instead of binding a new server. */
  replacing?: boolean;
}
export interface ServerSyncConnectPort {
  bind(config: ServerConfig): Promise<void>;
  reregister(config: ServerConfig): Promise<void>;
  synchronize(): Promise<void>;
}

/**
 * Registers the device, stores the asset policy, then runs the first sync.
 * The policy is written after binding because native refuses a remote policy
 * without a bound server, and before the first cycle because the engine reads
 * it at sync time: a remote device then receives asset metadata instead of
 * every asset body.
 */
export async function connectServerSync(
  controller: ServerSyncConnectPort,
  setAssetResidencyPolicy: (policy: AssetResidencyPolicy) => Promise<unknown>,
  request: ServerSyncConnectRequest,
): Promise<void> {
  if (request.replacing) await controller.reregister(request.config);
  else await controller.bind(request.config);
  await setAssetResidencyPolicy(request.residency);
  await controller.synchronize();
}

export function serverSyncRefreshRequired(error: string): boolean {
  return (
    error === "committed-refresh-pending" ||
    error === "activation-confirmation-pending"
  );
}

/** The sentence shown for a failed action or attempt. */
export function serverSyncErrorHelp(code: string, text: ServerSyncText): string {
  switch (code) {
    case "activation-confirmation-pending":
      return text.activationHelp;
    case "committed-refresh-pending":
      return text.refreshHelp;
    case "device-credential-unavailable":
      return text.credentialUnavailable;
    default:
      return text.errorHelp;
  }
}

export type ServerSyncStatusTone = "idle" | "connected" | "working" | "attention";
export interface ServerSyncStatusView {
  label: string;
  tone: ServerSyncStatusTone;
}

/** The one-line state shown beside the section title. */
export function serverSyncStatus(
  snapshot: ServerSyncSnapshot,
  text: ServerSyncText,
  actionError = "",
): ServerSyncStatusView {
  const error = actionError || snapshot.error;
  if (snapshot.status?.registrationRequired)
    return { label: text.registrationRequired, tone: "attention" };
  if (serverSyncRefreshRequired(error))
    return { label: text.refreshPending, tone: "attention" };
  if (snapshot.result?.phase === "conflict")
    return { label: text.conflict, tone: "attention" };
  if (snapshot.running)
    return {
      label: snapshot.progress ? text.progress[snapshot.progress] : text.running,
      tone: "working",
    };
  if (snapshot.paused) return { label: text.paused, tone: "connected" };
  if (snapshot.status?.operationPending)
    return { label: text.pending, tone: "connected" };
  if (snapshot.status?.configured)
    return { label: text.ready, tone: "connected" };
  return { label: text.disconnected, tone: "idle" };
}

export const SERVER_SYNC_STAGES: readonly ServerSyncProgress[] = [
  "saving",
  "preparing",
  "applying",
  "refreshing",
  "publishing",
];
export type ServerSyncStageState = "done" | "active" | "pending";
export interface ServerSyncStageView {
  stage: ServerSyncProgress;
  label: string;
  state: ServerSyncStageState;
  detail: string;
}
export interface ServerSyncCounterView {
  key: "bytes" | "rate" | "items" | "pending";
  label: string;
  value: string;
}
export interface ServerSyncProgressView {
  /** Share of this cycle's records processed, or null before the count is known. */
  percent: number | null;
  /** The active stage, with its record count when there is one. */
  current: string;
  elapsed: string;
  stages: ServerSyncStageView[];
  counters: ServerSyncCounterView[];
}

const NO_VALUE = "-";
const formatCount = (value: number): string => value.toLocaleString();
const fill = (template: string, value: string): string =>
  template.replace("{0}", value);

/** Everything a progress panel or summary row shows during an attempt. */
export function serverSyncProgressView(
  snapshot: ServerSyncSnapshot,
  text: ServerSyncText,
  now: number,
): ServerSyncProgressView {
  const items = snapshot.cycleItems;
  const counted = items !== undefined && items.total > 0;
  const activeIndex = snapshot.progress
    ? SERVER_SYNC_STAGES.indexOf(snapshot.progress)
    : 0;
  const ratio = counted ? `${formatCount(items.done)} / ${formatCount(items.total)}` : "";
  const stages = SERVER_SYNC_STAGES.map((stage, index): ServerSyncStageView => {
    const state: ServerSyncStageState =
      index < activeIndex ? "done" : index === activeIndex ? "active" : "pending";
    let detail = "";
    if (counted && stage === "preparing" && state === "done")
      detail = fill(text.itemsCount, formatCount(items.total));
    else if (
      counted &&
      state === "active" &&
      (stage === "applying" || stage === "publishing")
    )
      detail = ratio;
    return { stage, label: text.progress[stage], state, detail };
  });
  const active = stages[activeIndex];
  const bytes =
    snapshot.verifiedBytes === undefined
      ? NO_VALUE
      : formatRisuNestStorageBytes(Number(snapshot.verifiedBytes));
  const rate =
    snapshot.bytesPerSecond === undefined
      ? NO_VALUE
      : `${formatRisuNestStorageBytes(snapshot.bytesPerSecond)}/s`;
  const pending = snapshot.status
    ? snapshot.status.fullScan
      ? text.initialScan
      : fill(text.count, formatCount(snapshot.status.dirtyRecords))
    : NO_VALUE;
  return {
    percent: counted ? Math.min(100, Math.round((items.done / items.total) * 100)) : null,
    current: active.detail ? `${active.label} · ${active.detail}` : active.label,
    elapsed:
      snapshot.attemptStartedAt === undefined
        ? ""
        : `${text.elapsed} ${formatElapsed(now - snapshot.attemptStartedAt)}`,
    stages,
    counters: [
      { key: "bytes", label: text.verifiedBytes, value: bytes },
      { key: "rate", label: text.transferRate, value: rate },
      { key: "items", label: text.progressItems, value: counted ? ratio : NO_VALUE },
      { key: "pending", label: text.pendingChanges, value: pending },
    ],
  };
}

/** The server address without its scheme, for a compact chip. */
export function serverSyncHostLabel(endpoint: string): string {
  try {
    const url = new URL(endpoint);
    return `${url.host}${url.pathname.replace(/\/+$/, "")}`;
  } catch {
    return endpoint;
  }
}
