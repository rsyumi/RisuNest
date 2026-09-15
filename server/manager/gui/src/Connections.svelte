<script lang="ts">
  import { onDestroy, untrack, tick } from "svelte";
  import { Copy } from "@lucide/svelte";
  import { phase, type Status, type Environment } from "./api";
  let {
    status,
    environment,
    busy,
    mutate,
    copy,
    activity,
  }: {
    status: Status;
    environment: Environment | null;
    busy: boolean;
    mutate: (
      path: string,
      body?: Record<string, unknown>,
    ) => Promise<string | null>;
    copy: (text: string) => void;
    activity: (active: boolean) => void;
  } = $props();
  // Form drafts retain the revision they were based on while status polls continue.
  let mode = $state(
    untrack(() =>
      status.connectionState.mode === "managed" ? "managed" : "fixed",
    ),
  );
  let endpoint = $state(
    untrack(() =>
      status.connectionState.mode === "managed"
        ? ""
        : (status.connection.endpoint ?? ""),
    ),
  );
  let executable = $state(
    untrack(
      () => status.connection.cloudflared ?? environment?.cloudflared ?? "",
    ),
  );
  let registry = $state(
    untrack(
      () => status.connection.registryUrl ?? status.defaultRegistryUrl ?? "",
    ),
  );
  let enabled = $state(untrack(() => status.connection.registryEnabled));
  let revision = $state(untrack(() => status.revision));
  let dirty = $state(false);
  function changed() {
    dirty = true;
    activity(true);
  }
  function reset() {
    mode = status.connectionState.mode === "managed" ? "managed" : "fixed";
    endpoint = mode === "fixed" ? (status.connection.endpoint ?? "") : "";
    executable =
      status.connection.cloudflared ?? environment?.cloudflared ?? "";
    registry = status.connection.registryUrl ?? status.defaultRegistryUrl ?? "";
    enabled = status.connection.registryEnabled;
    revision = status.revision;
    dirty = false;
    activity(false);
  }
  async function apply() {
    const nextRevision = await mutate("connection", {
      revision,
      options: {
        endpoint: mode === "fixed" ? endpoint : null,
        cloudflared: mode === "managed" ? executable : null,
        registryUrl: enabled ? registry : null,
      },
    });
    if (nextRevision) {
      revision = nextRevision;
      await tick();
      reset();
    }
  }
  async function act(path: string) {
    const nextRevision = await mutate(path);
    if (nextRevision) revision = nextRevision;
  }
  onDestroy(() => activity(false));
</script>

<p class="page-description">
  고정 주소와 임시 주소, 주소 레지스트리를 설정합니다.
</p>
<form
  onsubmit={(e) => {
    e.preventDefault();
    void apply();
  }}
>
  <fieldset disabled={busy} class="card settings-group">
    <div class="setting">
      <label
        >연결 방식<select bind:value={mode} onchange={changed}
          ><option value="fixed">고정 주소</option><option value="managed"
            >임시 주소</option
          ></select
        ></label
      >
      <p>선택한 방식의 주소를 기기 등록과 레지스트리 게시에 사용합니다.</p>
    </div>
    {#if mode === "fixed"}<div class="setting">
        <label
          >고정 주소<input
            type="url"
            required
            bind:value={endpoint}
            oninput={changed}
            placeholder="https://"
          /></label
        >
        <p>등록된 기기가 이 주소로 연결합니다.</p>
      </div>{/if}
    <div class="setting">
      <div class="setting-heading">
        <h2>임시 주소</h2>
        <span class="pill">{phase(status.tunnel.phase)}</span>
      </div>
      <label
        >제공자<select
          aria-label="임시 주소 제공자"
          disabled={mode !== "managed"}
          ><option>Cloudflare Tunnel</option></select
        ></label
      >
      <p>
        인터넷을 통해 연결할 수 있는 주소를 발급합니다. 다시 시작하면 주소가
        바뀔 수 있습니다.
      </p>
      {#if status.tunnel.endpoint}<label
          >현재 임시 주소
          <div class="copy-field">
            <input readonly value={status.tunnel.endpoint} /><button
              type="button"
              class="icon-button"
              aria-label="임시 주소 복사"
              onclick={() => copy(status.tunnel.endpoint!)}
              ><Copy size={17} /></button
            >
          </div></label
        >{:else}<p>할당된 임시 주소가 없습니다.</p>{/if}
      {#if status.connectionState.mode === "managed"}<div class="actions">
          <button
            type="button"
            onclick={() => void act("tunnel/start")}
            disabled={status.tunnel.phase === "connected" || busy}>시작</button
          ><button type="button" onclick={() => void act("tunnel/restart")}
            >다시 시작</button
          ><button
            type="button"
            onclick={() => void act("tunnel/stop")}
            disabled={status.tunnel.phase === "stopped" || busy}>중지</button
          >
        </div>{/if}
      {#if mode === "managed"}<details>
          <summary>실행 파일 설정</summary><label
            >cloudflared 실행 파일<input
              bind:value={executable}
              oninput={changed}
              required
            /></label
          >
        </details>{/if}
    </div>
    <div class="setting">
      <div class="setting-heading">
        <h2>주소 레지스트리</h2>
        <label class="switch"
          ><input
            type="checkbox"
            bind:checked={enabled}
            onchange={changed}
            aria-label="레지스트리 사용"
          /><span></span></label
        >
      </div>
      <p>
        {phase(status.publication.phase)}. 주소가 바뀌면 자동으로 갱신합니다.
      </p>
      <div class="actions">
        <button
          type="button"
          disabled={!status.connection.registryEnabled || busy}
          onclick={() => void act("registry/repost")}>다시 게시</button
        >
      </div>
      <details class="advanced">
        <summary>고급 설정 <small>레지스트리 주소 · UUID</small></summary><label
          >레지스트리 서버 주소<input
            type="url"
            bind:value={registry}
            oninput={changed}
            required={enabled}
          /></label
        >
        <p>다른 레지스트리를 사용하려면 주소를 변경하세요.</p>
        <button
          class="text-button"
          type="button"
          disabled={!status.defaultRegistryUrl || busy}
          onclick={() => {
            registry = status.defaultRegistryUrl ?? "";
            changed();
          }}>기본 주소로 되돌리기</button
        >
        <p>이미 등록한 기기의 레지스트리 주소는 자동으로 바뀌지 않습니다.</p>
        <label
          >현재 UUID
          <div class="copy-field">
            <input
              readonly
              value={status.connection.uuid ?? "아직 발급되지 않음"}
            /><button
              type="button"
              class="icon-button"
              disabled={!status.connection.uuid || busy}
              aria-label="UUID 복사"
              onclick={() => copy(status.connection.uuid!)}
              ><Copy size={17} /></button
            >
          </div></label
        >
      </details>
    </div>
  </fieldset>
  <div class="form-footer">
    <div>
      <p>
        {dirty ? "아직 적용하지 않은 변경이 있습니다." : "현재 설정입니다."} 주소
        변경 시 기기 연결이 잠시 끊길 수 있습니다.
      </p>
      <button type="button" class="text-button" disabled={busy} onclick={reset}
        >서버 설정 다시 불러오기</button
      >
    </div>
    <button type="submit" class="primary" disabled={busy}>설정 적용</button>
  </div>
</form>
