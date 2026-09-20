<script lang="ts">
  import { onDestroy, tick, untrack } from "svelte";
  import type { NetworkSettings } from "./api";
  let { settings, listener, busy, save, activity }: {
    settings: NetworkSettings;
    listener: string | null;
    busy: boolean;
    save: (settings: NetworkSettings) => Promise<boolean>;
    activity: (dirty: boolean) => void;
  } = $props();
  let address = $state(untrack(() => settings.address));
  let port = $state<number | undefined>(untrack(() => settings.port));
  let dirty = $derived(address !== settings.address || port !== settings.port);
  $effect(() => activity(dirty));
  onDestroy(() => activity(false));
  async function apply() {
    if (!port) return;
    if (await save({ schema: 1, address: address.trim(), port })) {
      await tick();
      address = settings.address;
      port = settings.port;
    }
  }
</script>

<form onsubmit={(event) => { event.preventDefault(); void apply(); }}>
  <fieldset class="card settings-group" disabled={busy}>
    <div class="setting">
      <div class="setting-heading">
        <h2>네트워크 설정</h2>
        {#if listener}<span class="pill">수신 중</span>{/if}
      </div>
      {#if listener}<p>현재 수신 주소: <code>{listener}</code></p>{/if}
      <label>바인딩 주소<input aria-label="바인딩 주소" required bind:value={address} placeholder="127.0.0.1" /></label>
      <label>포트<input aria-label="포트" type="number" min="1" max="65535" step="1" required bind:value={port} /></label>
      <p>127.0.0.1은 이 컴퓨터에서만 연결할 수 있으며, 0.0.0.0은 모든 IPv4 네트워크에서 연결을 받습니다. IPv6 주소도 입력할 수 있습니다.</p>
      <p>다음 서버 시작부터 적용됩니다. 외부 연결에 HTTPS를 사용하려면 프록시 또는 Cloudflare Tunnel을 설정하세요.</p>
      <div class="actions"><button type="submit" disabled={busy || !dirty}>네트워크 설정 저장</button></div>
    </div>
  </fieldset>
</form>
