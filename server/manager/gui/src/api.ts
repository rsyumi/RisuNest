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
export interface NetworkSettings { schema: number; address: string; port: number; }
export interface Status {
  listener: string;
  revision: string;
  uptimeSeconds: number;
  connection: Connection;
  connectionState: { mode: string; publication: string };
  tunnel: { phase: string; endpoint: string | null; error: string | null; logs: string[] };
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
  actionMatches: boolean;
}
export interface Environment {
  network: NetworkSettings;
  platform: string;
  cloudflared: string;
  dataDir: string;
  startup: Startup | null;
  startupError: string | null;
  trayStartup: boolean;
  updateSettings: { schema: string; policy: "automatic" | "notify" | "off" };
  updateStatus: {
    schema: string;
    phase: string;
    targetVersion: string | null;
    lastCheckedAt: number | null;
    lastCompletedAt: number | null;
    deferredUntil: number | null;
    reason: string | null;
    lastFailedVersion: string | null;
  };
  updateSchedule: Startup | null;
  updateScheduleError: string | null;
}
export interface UpdateCheckOutcome {
  result: "skipped" | "current" | "available" | "deferred" | "started" | "completed";
  value?: string;
}
export interface Backend {
  status(): Promise<Status>;
  mutate(path: string, body: Record<string, unknown>): Promise<unknown>;
  environment(): Promise<Environment>;
  start(): Promise<void>;
  network(settings: NetworkSettings): Promise<void>;
  startup(action: "install" | "remove"): Promise<Startup>;
  trayStartup(enabled: boolean): Promise<void>;
  updatePolicy(policy: "automatic" | "notify" | "off"): Promise<void>;
  updateCheck(automatic: boolean): Promise<UpdateCheckOutcome>;
  requestId(): Promise<string>;
  qr(uri: string): Promise<string>;
}
export const native: Backend = {
  status: () => invoke("manager_status"),
  mutate: (path, body) => invoke("manager_mutate", { path, body }),
  environment: () => invoke("manager_environment"),
  start: () => invoke("manager_start"),
  network: (settings) => invoke("manager_network", { settings }),
  startup: (action) => invoke("manager_startup", { action }),
  trayStartup: (enabled) => invoke("manager_tray_startup", { enabled }),
  updatePolicy: (policy) => invoke("manager_update_policy", { policy }),
  updateCheck: (automatic) => invoke("manager_update_check", { automatic }),
  requestId: () => invoke("manager_request_id"),
  qr: (uri) => invoke("manager_qr", { uri }),
};
export function formatBytes(n: number | null): string {
  if (n === null) return "측정 중";
  if (n >= 1024 ** 3) return `${(n / 1024 ** 3).toFixed(2)} GiB`;
  if (n >= 1024 ** 2) return `${(n / 1024 ** 2).toFixed(1)} MiB`;
  return `${n.toLocaleString()} B`;
}
export function updatePhase(value: string): string {
  return (
    (
      {
        idle: "대기 중",
        checking: "확인 중",
        downloading: "다운로드 중",
        "waiting-idle": "적용 대기 중",
        draining: "서버 종료 중",
        installing: "설치 중",
        restarting: "다시 시작 중",
        completed: "완료",
        deferred: "연기됨",
        failed: "실패",
      } as Record<string, string>
    )[value] ?? value
  );
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
    "invalid-network-settings": "바인딩 IP 주소와 포트(1~65535)를 확인하세요.",
    "listen-address-in-use": "주소와 포트를 이미 사용 중입니다. 네트워크 설정에서 포트를 변경하세요.",
    "listen-address-unavailable": "이 컴퓨터에 없는 IP 주소입니다. 네트워크 설정을 확인하세요.",
    "listen-permission-denied": "설정한 주소와 포트에서 서버를 실행할 권한이 없습니다. 네트워크 설정을 확인하세요.",
    "tunnel-start-failed": "cloudflared를 실행하지 못했습니다. 실행 파일과 출력 내용을 확인하세요.",
    "tunnel-exited": "cloudflared가 종료되었습니다. 출력 내용을 확인하세요.",
    "tunnel-readiness-timeout": "90초 안에 임시 주소 연결을 완료하지 못했습니다. 네트워크와 출력 내용을 확인하세요.",
    "tunnel-job-unavailable": "cloudflared 프로세스를 관리하지 못했습니다. 출력 내용을 확인하세요.",
    "tunnel-output-unavailable": "cloudflared 출력을 읽지 못했습니다.",
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
