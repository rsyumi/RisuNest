<script lang="ts">
  import { onMount } from "svelte";
  import { language } from "src/lang";
  import {
    getServerSyncController,
    listServerSyncBackups,
    restoreServerSyncBackup,
    type ServerSyncBackup,
  } from "src/ts/storage/sync/serverSyncProduction";
  import { serverSyncError } from "src/ts/storage/sync/serverSync";
  const controller = getServerSyncController();
  let snapshot = $state(controller.snapshot());
  let endpoint = $state("");
  let libraryId = $state("");
  let deviceId = $state("");
  let token = $state("");
  let connecting = $state(false);
  let actionError = $state("");
  let replacing = $state(false);
  let backups = $state<ServerSyncBackup[]>([]);
  let backupsVisible = $state(false);
  const text = $derived(language.risuNest.serverSync);
  const error = $derived(actionError || snapshot.error);
  const refreshRequired = $derived(
    error === "committed-refresh-pending" ||
      error === "activation-confirmation-pending",
  );
  const conflict = $derived(
    snapshot.result?.phase === "conflict" ? snapshot.result : undefined,
  );
  const busy = $derived(connecting || snapshot.running);
  const status = $derived(
    snapshot.status?.registrationRequired
      ? text.registrationRequired
      : refreshRequired
        ? text.refreshPending
        : conflict
          ? text.conflict
          : snapshot.running
            ? snapshot.progress
              ? text.progress[snapshot.progress]
              : text.running
            : snapshot.paused
              ? text.paused
              : snapshot.status?.operationPending
                ? text.pending
                : snapshot.status?.configured
                  ? text.ready
                  : text.disconnected,
  );
  onMount(() => {
    const unsubscribe = controller.subscribe((value) => {
      snapshot = value;
    });
    void controller.initialize();
    return unsubscribe;
  });
  async function connect(): Promise<void> {
    if (busy) return;
    connecting = true;
    actionError = "";
    try {
      const config = {
        endpoint: endpoint.trim(),
        libraryId: libraryId.trim(),
        deviceId: deviceId.trim(),
        token: token.trim(),
      };
      if (replacing) await controller.reregister(config);
      else await controller.bind(config);
      token = "";
      replacing = false;
      await controller.synchronize();
    } catch (cause) {
      actionError = serverSyncError(cause).code;
    } finally {
      connecting = false;
    }
  }
  async function disconnect(): Promise<void> {
    actionError = "";
    try {
      await controller.unbind();
    } catch (cause) {
      actionError = serverSyncError(cause).code;
    }
  }
  async function reconcile(): Promise<void> {
    connecting = true;
    actionError = "";
    try {
      await controller.reconcile();
      await controller.synchronize();
    } catch (cause) {
      actionError = serverSyncError(cause).code;
    } finally {
      connecting = false;
    }
  }
  async function showBackups(): Promise<void> {
    connecting = true;
    actionError = "";
    try {
      backups = await listServerSyncBackups();
      backupsVisible = true;
    } catch (cause) {
      actionError = serverSyncError(cause).code;
    } finally {
      connecting = false;
    }
  }
  async function restoreBackup(
    id: string,
    side: "local" | "remote",
  ): Promise<void> {
    connecting = true;
    actionError = "";
    try {
      await restoreServerSyncBackup(id, side);
    } catch (cause) {
      actionError = serverSyncError(cause).code;
    } finally {
      connecting = false;
    }
  }
  function resolve(resolution: "keep-local" | "keep-remote"): void {
    if (!conflict) return;
    void controller.synchronize({
      resolution,
      expectedRevision: conflict.localRevision,
      expectedHead: conflict.head,
    });
  }
</script>

<section class="server-sync text-textcolor" aria-labelledby="server-sync-title">
  <div class="flex flex-wrap items-center justify-between gap-3">
    <h2 id="server-sync-title" class="text-2xl font-bold">{text.title}</h2>
    <span class="status border border-darkborderc" aria-live="polite">
      <span
        class:working={snapshot.running}
        class:connected={snapshot.status?.configured}
        class="status-dot"
        aria-hidden="true"
      ></span>
      {status}
    </span>
  </div>
  <p class="text-sm opacity-75">{text.description}</p>
  {#if snapshot.status?.configured}
    <div class="connection bg-darkbg border border-darkborderc">
      <div class="min-w-0">
        <p class="text-xs opacity-65">{text.endpoint}</p>
        <p class="break-all font-medium">{snapshot.status.endpoint}</p>
      </div>
      <p class="text-sm opacity-75">
        {snapshot.status.fullScan
          ? text.initialScan
          : text.queued.replace("{0}", String(snapshot.status.dirtyRecords))}
      </p>
      <p class="text-xs opacity-65">
        {text.deviceId}: {snapshot.status.deviceId}
      </p>
      {#if snapshot.lastSuccessAt !== undefined}
        <p class="text-xs opacity-65">
          {text.lastSuccess}: {new Date(
            snapshot.lastSuccessAt,
          ).toLocaleString()}
        </p>
      {/if}
      {#if snapshot.verifiedBytes !== undefined}
        <p class="text-xs opacity-65">
          {text.verifiedBytes}: {BigInt(
            snapshot.verifiedBytes,
          ).toLocaleString()} B
        </p>
      {/if}
    </div>
    <div class="flex flex-wrap gap-2">
      <button
        class="action bg-darkbutton border border-darkborderc hover:bg-selected"
        disabled={busy || snapshot.status.registrationRequired}
        onclick={() => void controller.synchronize()}
        >{refreshRequired ? text.refresh : text.syncNow}</button
      >
      <button
        class="action border border-darkborderc hover:bg-selected"
        disabled={connecting || snapshot.paused || refreshRequired}
        onclick={() => void controller.pause()}>{text.pause}</button
      >
      <button
        class="action border border-darkborderc hover:bg-selected"
        disabled={busy || snapshot.status.operationPending || refreshRequired}
        onclick={() => void disconnect()}>{text.disconnect}</button
      >
    </div>
    {#if snapshot.status.operationPending}<p class="text-sm opacity-75">
        {text.pendingHelp}
      </p>{/if}
    {#if !replacing}
      <button
        class="action border border-darkborderc hover:bg-selected justify-self-start"
        disabled={busy || refreshRequired}
        onclick={() => {
          endpoint = snapshot.status?.endpoint ?? "";
          libraryId = snapshot.status?.libraryId ?? "";
          deviceId = "";
          replacing = true;
        }}>{text.reregister}</button
      >
    {/if}
    {#if error === "epoch-reconciliation-required"}
      <p class="text-sm opacity-75">{text.reconcileHelp}</p>
      <button
        class="action bg-darkbutton border border-darkborderc hover:bg-selected justify-self-start"
        disabled={busy || refreshRequired}
        onclick={() => void reconcile()}>{text.reconcile}</button
      >
    {/if}
  {/if}
  {#if !snapshot.status?.configured || replacing}
    {#if replacing}<p class="text-sm opacity-75">{text.reregisterHelp}</p>{/if}
    <form
      class="connection-form"
      onsubmit={(event) => {
        event.preventDefault();
        void connect();
      }}
    >
      <label class="field"
        ><span>{text.endpoint}</span><input
          type="url"
          bind:value={endpoint}
          placeholder="https://sync.example.com"
          required
          autocomplete="url"
          disabled={busy}
        /></label
      >
      <div class="identity-fields">
        <label class="field"
          ><span>{text.libraryId}</span><input
            bind:value={libraryId}
            required
            autocomplete="off"
            autocapitalize="none"
            spellcheck="false"
            disabled={busy}
          /></label
        >
        <label class="field"
          ><span>{text.deviceId}</span><input
            bind:value={deviceId}
            required
            autocomplete="off"
            autocapitalize="none"
            spellcheck="false"
            disabled={busy}
          /></label
        >
      </div>
      <label class="field"
        ><span>{text.token}</span><input
          type="password"
          bind:value={token}
          required
          autocomplete="new-password"
          spellcheck="false"
          disabled={busy}
        /></label
      >
      <p class="text-sm opacity-75">{text.credentialsHelp}</p>
      <button
        class="action bg-darkbutton border border-darkborderc hover:bg-selected justify-self-start"
        type="submit"
        disabled={busy}>{replacing ? text.reregister : text.connect}</button
      >
    </form>
  {/if}
  {#if conflict}
    <div class="conflict border border-darkborderc" role="status">
      <h3 class="font-bold">
        {text.conflictCount.replace("{0}", String(conflict.conflictCount))}
      </h3>
      <p class="text-sm opacity-80">{text.conflictHelp}</p>
      <div class="flex flex-wrap gap-2">
        <button
          class="action bg-darkbutton border border-darkborderc hover:bg-selected"
          disabled={busy}
          onclick={() => resolve("keep-local")}>{text.keepLocal}</button
        >
        <button
          class="action bg-darkbutton border border-darkborderc hover:bg-selected"
          disabled={busy}
          onclick={() => resolve("keep-remote")}>{text.keepRemote}</button
        >
      </div>
    </div>
  {/if}
  {#if error && error !== "cancelled"}
    <p class="text-sm" role="alert">
      {error === "activation-confirmation-pending"
        ? text.activationHelp
        : error === "committed-refresh-pending"
          ? text.refreshHelp
          : error === "device-credential-unavailable"
            ? text.credentialUnavailable
            : text.errorHelp} <span class="opacity-60">({error})</span>
    </p>
  {/if}
  <button
    class="action border border-darkborderc hover:bg-selected justify-self-start"
    disabled={busy || refreshRequired}
    onclick={() => void showBackups()}>{text.backups}</button
  >
  {#if backupsVisible}
    <div class="connection bg-darkbg border border-darkborderc">
      <p class="text-sm opacity-75">{text.backupHelp}</p>
      {#if backups.length === 0}<p class="text-sm">{text.noBackups}</p>{/if}
      {#each backups as backup (backup.id)}
        <div class="grid gap-2">
          <p class="text-sm">{new Date(backup.createdAt).toLocaleString()}</p>
          <div class="flex flex-wrap gap-2">
            <button
              class="action border border-darkborderc hover:bg-selected"
              disabled={busy ||
                refreshRequired ||
                snapshot.status?.operationPending}
              onclick={() => void restoreBackup(backup.id, "local")}
              >{text.restoreLocalBackup}</button
            >
            <button
              class="action border border-darkborderc hover:bg-selected"
              disabled={busy ||
                refreshRequired ||
                snapshot.status?.operationPending}
              onclick={() => void restoreBackup(backup.id, "remote")}
              >{text.restoreRemoteBackup}</button
            >
          </div>
        </div>
      {/each}
    </div>
  {/if}
</section>

<style>
  .server-sync {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    min-width: 0;
    overflow-wrap: anywhere;
    gap: 1rem;
    margin-block: 2rem;
  }
  .status {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    border-radius: 99px;
    padding: 0.35rem 0.75rem;
    font-size: 0.75rem;
  }
  .status-dot {
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 50%;
    background: currentColor;
    opacity: 0.35;
  }
  .status-dot.connected {
    opacity: 1;
  }
  .status-dot.working {
    animation: pulse 1.5s ease-in-out infinite;
  }
  .connection {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 0.75rem;
    border-radius: 0.5rem;
    padding: 1rem;
  }
  .connection-form,
  .field {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 0.5rem;
  }
  .connection-form {
    gap: 1rem;
  }
  .field > span {
    font-size: 0.875rem;
    font-weight: 500;
  }
  .field input {
    color: inherit;
    background: transparent;
    border: 1px solid var(--risu-theme-darkborderc);
    border-radius: 0.35rem;
    padding: 0.65rem 0.75rem;
    min-width: 0;
    width: 100%;
  }
  .identity-fields {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 1rem;
  }
  .action {
    border-radius: 0.35rem;
    padding: 0.6rem 0.9rem;
    font-size: 0.875rem;
    transition: background-color 0.15s;
  }
  .action:disabled,
  input:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .action:focus-visible,
  input:focus-visible {
    outline: 2px solid currentColor;
    outline-offset: 3px;
  }
  .conflict {
    display: grid;
    gap: 0.75rem;
    border-radius: 0.5rem;
    padding: 1rem;
    border-left-width: 3px;
  }
  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .status-dot.working {
      animation: none;
    }
  }
  @media (max-width: 480px) {
    .identity-fields {
      grid-template-columns: 1fr;
    }
  }
</style>
