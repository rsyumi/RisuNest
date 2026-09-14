<script lang="ts">
  import { Check, Copy, ChevronRight, AlertCircle } from "@lucide/svelte";
  import { formatBytes, phase, type Status } from "./api";
  let {
    status,
    connected,
    copy,
    showDevices,
  }: {
    status: Status;
    connected: boolean;
    copy: (text: string) => void;
    showDevices: () => void;
  } = $props();
  const active = $derived(status.devices.filter((d) => !d.revoked));
  const unmeasured = $derived(
    status.storage.error ? "측정할 수 없음" : "측정 중",
  );
  const address = $derived(
    status.connectionState.mode === "managed"
      ? status.tunnel.endpoint
      : status.connection.endpoint,
  );
</script>

<section class="card health">
  <div class="health-heading">
    <span class="health-icon" class:offline={!connected}
      >{#if connected}<Check size={20} />{:else}<AlertCircle size={20} />{/if}</span
    >
    <div>
      <h2>{connected ? "서버 실행 중" : "마지막으로 확인한 서버 정보"}</h2>
      <p>
        {!connected
          ? "연결이 끊기기 전 상태입니다."
          : address
          ? "등록된 기기와 동기화할 수 있습니다."
          : "연결 설정에서 서버 주소를 준비하세요."}
      </p>
    </div>
    <small class="uptime"
      >실행 시간<br />{Math.floor(status.uptimeSeconds / 3600)}시간 {Math.floor(
        (status.uptimeSeconds % 3600) / 60,
      )}분</small
    >
  </div>
  {#if address}<div class="copy-field">
      <input readonly aria-label="현재 서버 주소" value={address} /><button
        class="icon-button"
        aria-label="서버 주소 복사"
        onclick={() => copy(address!)}><Copy size={17} /></button
      >
    </div>{/if}
  <div class="services">
    <span>● 동기화 서버</span><span
      >임시 주소 · {phase(status.tunnel.phase)}</span
    ><span>레지스트리 · {phase(status.publication.phase)}</span>
  </div>
  {#if status.tunnel.error || status.publication.error}<p class="warning">
      <AlertCircle size={15} /> 연결 상태를 확인하세요. 서버에 저장된 데이터는 유지됩니다.
    </p>{/if}
</section>
<section class="card storage-summary" aria-label="저장 공간">
  <div class="storage-metrics">
    <div>
      <span>서버 파일 용량</span><strong
        >{status.storage.totalBytes === null
          ? unmeasured
          : formatBytes(status.storage.totalBytes)}</strong
      >
    </div>
    <div>
      <span>드라이브 남은 공간</span><strong
        >{status.storage.availableBytes === null
          ? "조회할 수 없음"
          : formatBytes(status.storage.availableBytes)}</strong
      >
    </div>
  </div>
  <details>
    <summary
      >용량 상세 <small
        >{status.storage.measuredAt
          ? new Date(status.storage.measuredAt * 1000).toLocaleTimeString() +
            " 확인"
          : unmeasured}</small
      ></summary
    >
    <dl>
      {#each [["저장 데이터", status.storage.dataBytes], ["데이터베이스", status.storage.databaseBytes], ["임시 파일", status.storage.temporaryBytes], ["기타", status.storage.otherBytes]] as [name, value]}<div
        >
          <dt>{name}</dt>
          <dd>
            {status.storage.measuredAt
              ? formatBytes(value as number)
              : unmeasured}
          </dd>
        </div>{/each}
    </dl>
    <p>
      서버 데이터 폴더의 파일 크기 합계입니다. 별도 백업 폴더는 포함하지
      않습니다.
    </p>
  </details>
  {#if status.storage.error}<p class="notice">
      용량을 새로 확인하지 못했습니다.{status.storage.measuredAt
        ? " 마지막 측정값을 표시합니다."
        : " 다음 측정 때 다시 확인합니다."}
    </p>{/if}
</section>
<div class="section-heading">
  <h2>등록된 기기 <small>{active.length}</small></h2>
  <button class="text-button" onclick={showDevices}
    >모두 보기 <ChevronRight size={15} /></button
  >
</div>
<div class="card device-list">
  {#each active.slice(0, 3) as device}<div class="device-row">
      <div>
        <strong>{device.name || device.id.slice(0, 12)}</strong><small
          >{device.id}</small
        >
      </div>
      <span class="pill">{device.pending ? "작업 처리 중" : "등록됨"}</span>
    </div>{:else}<p class="empty">
      등록된 기기가 없습니다. 기기 등록을 눌러 추가하세요.
    </p>{/each}
</div>
