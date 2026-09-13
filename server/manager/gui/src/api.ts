import { invoke } from "@tauri-apps/api/core";
export interface Connection {
  endpoint: string | null;
  cloudflared: string | null;
  registryUrl: string | null;
  registryEnabled: boolean;
  uuid: string | null;
}
export interface Device {
  id: string;
  name: string;
  revoked: boolean;
  pending: boolean;
  registrationRequest: string | null;
}
export interface Status {
  revision: string;
  uptimeSeconds: number;
  connection: Connection;
  connectionState: { mode: string; publication: string };
  tunnel: { phase: string; endpoint: string | null; error: string | null };
  publication: { phase: string; error: string | null };
  storage: {
    measuredAt: number | null;
    totalBytes: number | null;
    availableBytes: number | null;
    dataBytes: number;
    databaseBytes: number;
    temporaryBytes: number;
    otherBytes: number;
    error: string | null;
  };
  devices: Device[];
  defaultRegistryUrl: string | null;
}
export interface Startup {
  registered: boolean;
  enabled: boolean;
}
export interface Environment {
  platform: string;
  cloudflared: string;
  dataDir: string;
  startup: Startup | null;
  startupError: string | null;
  trayStartup: boolean;
}
export interface Backend {
  status(): Promise<Status>;
  mutate(path: string, body: Record<string, unknown>): Promise<unknown>;
  environment(): Promise<Environment>;
  start(): Promise<void>;
  startup(action: "install" | "remove"): Promise<Startup>;
  trayStartup(enabled: boolean): Promise<void>;
  requestId(): Promise<string>;
  qr(uri: string): Promise<string>;
}
export const native: Backend = {
  status: () => invoke("manager_status"),
  mutate: (path, body) => invoke("manager_mutate", { path, body }),
  environment: () => invoke("manager_environment"),
  start: () => invoke("manager_start"),
  startup: (action) => invoke("manager_startup", { action }),
  trayStartup: (enabled) => invoke("manager_tray_startup", { enabled }),
  requestId: () => invoke("manager_request_id"),
  qr: (uri) => invoke("manager_qr", { uri }),
};
export function formatBytes(n: number | null): string {
  if (n === null) return "측정 중";
  if (n >= 1024 ** 3) return `${(n / 1024 ** 3).toFixed(2)} GiB`;
  if (n >= 1024 ** 2) return `${(n / 1024 ** 2).toFixed(1)} MiB`;
  return `${n.toLocaleString()} B`;
}
export function phase(value: string): string {
  return (
    (
      {
        connected: "연결됨",
        starting: "준비 중",
        failed: "오류 발생",
        stopped: "중지됨",
        external: "사용 안 함",
        published: "게시 완료",
        publishing: "게시 중",
        pending: "게시 대기 중",
        waiting: "대기 중",
        disabled: "사용 안 함",
        "waiting-for-address": "주소 준비 중",
      } as Record<string, string>
    )[value] ?? value
  );
}
export function message(error: unknown): string {
  const code =
    typeof error === "string"
      ? error.split(":")[0]
      : "management-request-failed";
  const known: Record<string, string> = {
    "startup-registration-required":
      "실행 설정에서 서버 자동 실행을 등록한 뒤 서버를 시작하세요.",
    "management-stale-state":
      "서버 상태가 변경되었습니다. 새로 확인한 뒤 다시 시도하세요.",
    "registration-already-issued":
      "이미 발급한 요청입니다. 기기 목록을 확인하세요. 링크를 잃었다면 해당 기기를 해제한 뒤 다시 등록하세요.",
    "public-endpoint-not-ready":
      "서버 주소가 준비되지 않았습니다. 연결 설정을 확인하세요.",
    "invalid-device-name":
      "기기 이름을 확인하세요. 1~80자이며 제어 문자는 사용할 수 없습니다.",
    "absolute-cloudflared-executable-required":
      "cloudflared 실행 파일의 절대 경로를 확인하세요.",
    "management-response-incomplete":
      "응답을 끝까지 받지 못했습니다. 기기 목록과 설정을 확인한 뒤 다시 시도하세요.",
    "task-scheduler-operation-failed":
      "작업 스케줄러 설정을 변경하지 못했습니다. 사용자 권한과 시스템 정책을 확인하세요.",
  };
  return (
    known[code] ?? "작업을 완료하지 못했습니다. 서버 상태와 설정을 확인하세요."
  );
}
