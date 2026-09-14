<script lang="ts">
  import { onMount } from "svelte";
  import { language } from "src/lang";
  import SettingGroup from "../RisuNest/SettingGroup.svelte";
  import SettingRow from "../RisuNest/SettingRow.svelte";
  import SettingButton from "../RisuNest/SettingButton.svelte";
  import ServerAssetResidency from "./ServerAssetResidency.svelte";
  import ServerSyncConnect from "./ServerSyncConnect.svelte";
  import ServerSyncRegistrationInput from "./ServerSyncRegistrationInput.svelte";
  import ServerSyncStorage from "./ServerSyncStorage.svelte";
  import { formatRisuNestStorageBytes as bytes } from "src/ts/storage/risuNestStorageDashboard";
  import { setAssetResidencyPolicy } from "src/ts/storage/sync/serverAssetResidency";
  import { serverSyncError } from "src/ts/storage/sync/serverSync";
  import {
    connectServerSync,
    serverSyncErrorHelp,
    serverSyncProgressView,
    serverSyncRefreshRequired,
    serverSyncStatus,
    type ServerSyncConnectRequest,
  } from "src/ts/storage/sync/serverSyncConnectFlow";
  import type { ServerSyncNavigation } from "src/ts/storage/sync/serverSyncDeepLink";
  import {
    getServerSyncBackupInventory,
    getServerSyncCacheUsage,
    getServerSyncController,
    type ServerSyncCacheUsage,
  } from "src/ts/storage/sync/serverSyncProduction";

  /** The settings section: state, the connection, and the rows that manage it. */
  let {
    origin = "settings",
    initialNavigation,
  }: {
    origin?: "settings" | "deep-link";
    initialNavigation?: ServerSyncNavigation;
  } = $props();
  const controller = getServerSyncController();
  let snapshot = $state(controller.snapshot());
  let connecting = $state(false);
  let pausing = $state(false);
  let disconnecting = $state(false);
  let actionError = $state("");
  let connectOpen = $state(false);
  let connectStage = $state<"code" | "review">("code");
  let connectKey = $state(0);
  let replacingOpen = $state(false);
  let backupsOpen = $state(false);
  let storageOpen = $state(false);
  let backupCount = $state<number | undefined>();
  let cacheUsage = $state<ServerSyncCacheUsage | undefined>();
  let now = $state(Date.now());
  let appliedNavigation: ServerSyncNavigation | undefined;
  const text = $derived(language.risuNest.serverSync);
  const error = $derived(actionError || snapshot.error);
  const refreshRequired = $derived(serverSyncRefreshRequired(error));
  const configured = $derived(Boolean(snapshot.status?.configured));
  const conflict = $derived(
    snapshot.result?.phase === "conflict" ? snapshot.result : undefined,
  );
  const busy = $derived(
    connecting ||
      pausing ||
      disconnecting ||
      snapshot.running ||
      snapshot.replacing,
  );
  const status = $derived(serverSyncStatus(snapshot, text, actionError));
  const progress = $derived(
    snapshot.running ? serverSyncProgressView(snapshot, text, now) : undefined,
  );
  const pendingChanges = $derived(
    snapshot.status
      ? snapshot.status.fullScan
        ? text.initialScan
        : text.count.replace(
            "{0}",
            snapshot.status.dirtyRecords.toLocaleString(),
          )
      : "",
  );
  const backupsHelp = $derived(
    backupCount === undefined
      ? text.backupHelp
      : backupCount === 0
        ? text.noBackups
        : text.backupCount.replace("{0}", backupCount.toLocaleString()),
  );
  const storageHelp = $derived(
    cacheUsage
      ? `${text.management.cache} ${bytes(cacheUsage.totalBytes)} · ${text.management.reclaimable} ${bytes(cacheUsage.reclaimableBytes)}`
      : text.management.scope,
  );
  $effect(() => {
    if (!initialNavigation || appliedNavigation === initialNavigation) return;
    appliedNavigation = initialNavigation;
    if (!configured) connectOpen = true;
  });
  $effect(() => {
    // A code delivered while the block is folded still needs its check shown.
    if (connectStage === "review") connectOpen = true;
  });
  $effect(() => {
    if (!snapshot.running) return;
    now = Date.now();
    const timer = setInterval(() => {
      now = Date.now();
    }, 1000);
    return () => clearInterval(timer);
  });
  $effect(() => {
    // Row summaries follow every finished attempt.
    if (!snapshot.running) void loadSummaries();
  });
  onMount(() => {
    const unsubscribe = controller.subscribe((value) => {
      snapshot = value;
    });
    // Startup owns initialization; mounting a view must preserve its attempt.
    return unsubscribe;
  });
  async function loadSummaries(): Promise<void> {
    try {
      const [inventory, usage] = await Promise.all([
        getServerSyncBackupInventory(),
        getServerSyncCacheUsage(),
      ]);
      backupCount = inventory.completeCount;
      cacheUsage = usage;
    } catch {
      backupCount = undefined;
      cacheUsage = undefined;
    }
  }
  async function connect(request: ServerSyncConnectRequest): Promise<void> {
    if (busy) return;
    connecting = true;
    actionError = "";
    try {
      await connectServerSync(controller, setAssetResidencyPolicy, request);
      // The credentials are bound now; drop the copy the check screen held.
      connectStage = "code";
      connectKey++;
      connectOpen = false;
      replacingOpen = false;
    } catch (cause) {
      actionError = serverSyncError(cause).code;
    } finally {
      connecting = false;
    }
  }
  async function pause(): Promise<void> {
    if (pausing) return;
    pausing = true;
    actionError = "";
    try {
      await controller.pause();
      await controller.waitForIdle();
    } catch (cause) {
      actionError = serverSyncError(cause).code;
    } finally {
      pausing = false;
    }
  }
  async function disconnect(): Promise<void> {
    if (disconnecting) return;
    disconnecting = true;
    actionError = "";
    try {
      await controller.unbind();
    } catch (cause) {
      actionError = serverSyncError(cause).code;
    } finally {
      disconnecting = false;
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
  function resolve(resolution: "keep-local" | "keep-remote"): void {
    if (!conflict) return;
    void controller.synchronize({
      resolution,
      expectedRevision: conflict.localRevision,
      expectedHead: conflict.head,
    });
  }
</script>

<SettingGroup
  title={text.title}
  description={text.description}
  panelProps={{ "data-origin": origin }}
>
  {#snippet actions()}
    <span class="status border border-darkborderc" data-tone={status.tone} aria-live="polite">
      <span class="status-dot" aria-hidden="true"></span>
      {status.label}
    </span>
  {/snippet}
  {#if conflict}
    <div class="conflict m-4 mb-1" role="status">
      <h3 class="font-bold">
        {text.conflictCount.replace("{0}", String(conflict.conflictCount))}
      </h3>
      <p class="text-sm opacity-80">{text.conflictHelp}</p>
      <div class="flex flex-wrap gap-2">
        <SettingButton disabled={busy} onclick={() => resolve("keep-local")}
          >{text.keepLocal}</SettingButton
        >
        <SettingButton disabled={busy} onclick={() => resolve("keep-remote")}
          >{text.keepRemote}</SettingButton
        >
      </div>
    </div>
  {/if}
  {#if snapshot.status?.configured}
    <div class="summary px-4 py-3">
      <p class="break-all text-[15px] font-semibold">{snapshot.status.endpoint}</p>
      <dl class="kv">
        <dt>{text.libraryId}</dt>
        <dd>{snapshot.status.libraryId}</dd>
        <dt>{text.deviceId}</dt>
        <dd>{snapshot.status.deviceId}</dd>
        {#if progress}
          <dt>{text.progressLabel}</dt>
          <dd>
            {progress.current}{progress.percent === null
              ? ""
              : ` (${progress.percent}%)`}
          </dd>
          <dt>{text.verifiedBytes}</dt>
          <dd>{progress.counters[0].value} · {progress.counters[1].value}</dd>
          <dt>{text.pendingChanges}</dt>
          <dd>{pendingChanges}</dd>
          {#if progress.elapsed}
            <dt>{text.elapsed}</dt>
            <dd>{progress.elapsed.slice(text.elapsed.length).trim()}</dd>
          {/if}
        {:else}
          {#if snapshot.lastSuccessAt !== undefined}
            <dt>{text.lastSuccess}</dt>
            <dd>{new Date(snapshot.lastSuccessAt).toLocaleString()}</dd>
          {/if}
          <dt>{text.pendingChanges}</dt>
          <dd>{pendingChanges}</dd>
        {/if}
      </dl>
      {#if progress}
        <div
          class="thin"
          role="progressbar"
          aria-label={text.running}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={progress.percent ?? undefined}
        >
          <i
            class:pulse={progress.percent === null}
            style:width={progress.percent === null
              ? "100%"
              : `${progress.percent}%`}
          ></i>
        </div>
      {/if}
      {#if snapshot.running && snapshot.retryableFailure}
        <p class="mt-2 text-sm" role="status">
          {text.running}: <span class="opacity-75">({snapshot.retryableFailure})</span>
        </p>
      {/if}
      <div class="mt-3 flex flex-wrap gap-2">
        <SettingButton
          busy={snapshot.running}
          disabled={busy || snapshot.status.registrationRequired}
          onclick={() => void controller.synchronize()}
          >{refreshRequired ? text.refresh : text.syncNow}</SettingButton
        >
        <SettingButton
          variant="secondary"
          busy={pausing}
          disabled={connecting || snapshot.paused || refreshRequired}
          onclick={() => void pause()}>{text.pause}</SettingButton
        >
        <SettingButton
          variant="danger"
          busy={disconnecting}
          disabled={busy || snapshot.status.operationPending || refreshRequired}
          onclick={() => void disconnect()}>{text.disconnect}</SettingButton
        >
      </div>
      {#if snapshot.status.operationPending}<p class="mt-2 text-sm opacity-75">
          {text.pendingHelp}
        </p>{/if}
      {#if error === "epoch-reconciliation-required"}
        <p class="mt-2 text-sm opacity-75">{text.reconcileHelp}</p>
        <SettingButton
          class="mt-2"
          busy={connecting}
          disabled={busy || refreshRequired}
          onclick={() => void reconcile()}>{text.reconcile}</SettingButton
        >
      {/if}
      {#if error && error !== "cancelled" && !replacingOpen}
        <p class="mt-2 text-sm" role="alert">
          {serverSyncErrorHelp(error, text)} <span class="opacity-60">({error})</span>
        </p>
      {/if}
    </div>
    <ServerAssetResidency disabled={busy || refreshRequired} />
    <SettingRow label={text.reregister} help={text.reregisterHelp}>
      <SettingButton
        variant="secondary"
        disabled={busy || refreshRequired}
        aria-expanded={replacingOpen}
        onclick={() => {
          replacingOpen = !replacingOpen;
          connectKey++;
        }}>{text.register}</SettingButton
      >
    </SettingRow>
    {#if replacingOpen}
      <div class="px-4 py-3">
        {#key connectKey}
          <ServerSyncConnect
            replacing
            available
            {busy}
            error={actionError}
            initialNavigation={{
              endpoint: snapshot.status.endpoint ?? undefined,
              libraryId: snapshot.status.libraryId ?? undefined,
            }}
            onSubmit={(request) => void connect(request)}
          />
        {/key}
      </div>
    {:else}
      <div class="contents">
        <ServerSyncRegistrationInput available={false} onRegistration={() => {}} />
      </div>
    {/if}
  {:else}
    <SettingRow label={text.connectRow} help={text.connectRowHelp}>
      <SettingButton
        aria-expanded={connectOpen}
        onclick={() => {
          connectOpen = !connectOpen;
        }}>{text.enterCode}</SettingButton
      >
    </SettingRow>
    <div class="px-4 py-3" hidden={!connectOpen}>
      {#key connectKey}
        <ServerSyncConnect
          bind:stage={connectStage}
          {initialNavigation}
          {busy}
          error={actionError}
          onSubmit={(request) => void connect(request)}
        />
      {/key}
    </div>
  {/if}
  <SettingRow label={text.backups} help={backupsHelp}>
    <SettingButton
      variant="secondary"
      disabled={busy || refreshRequired}
      aria-expanded={backupsOpen}
      onclick={() => {
        backupsOpen = !backupsOpen;
      }}>{text.viewList}</SettingButton
    >
  </SettingRow>
  {#if backupsOpen}
    <div class="px-4">
      <ServerSyncStorage section="backups" onChange={() => void loadSummaries()} />
    </div>
  {/if}
  {#if snapshot.status?.configured}
    <SettingRow label={text.management.title} help={storageHelp}>
      <SettingButton
        variant="secondary"
        disabled={busy || refreshRequired}
        aria-expanded={storageOpen}
        onclick={() => {
          storageOpen = !storageOpen;
        }}>{text.viewList}</SettingButton
      >
    </SettingRow>
    {#if storageOpen}
      <div class="px-4">
        <ServerSyncStorage section="cache" onChange={() => void loadSummaries()} />
      </div>
    {/if}
  {/if}
</SettingGroup>

<style>
  .status {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    border-radius: 99px;
    padding: 0.35rem 0.75rem;
    font-size: 0.75rem;
  }
  .status[data-tone="attention"] {
    color: var(--risu-theme-danger-400);
    border-color: color-mix(in srgb, var(--risu-theme-danger-400) 50%, transparent);
  }
  .status-dot {
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 50%;
    background: currentColor;
    opacity: 0.35;
  }
  .status[data-tone="connected"] .status-dot {
    background: var(--risu-theme-success-500);
    opacity: 1;
  }
  .status[data-tone="attention"] .status-dot {
    opacity: 1;
  }
  .status[data-tone="paused"] .status-dot {
    background: transparent;
    box-shadow: inset 0 0 0 1.5px currentColor;
    opacity: 0.7;
  }
  .status[data-tone="working"] .status-dot {
    background: var(--risu-theme-primary-500);
    opacity: 1;
    animation: pulse 1.5s ease-in-out infinite;
  }
  .summary {
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .kv {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: 0.25rem 1rem;
    margin: 0.5rem 0 0;
    font-size: 0.8125rem;
  }
  .kv dt {
    color: color-mix(in srgb, var(--risu-theme-textcolor) 60%, transparent);
  }
  .kv dd {
    margin: 0;
    min-width: 0;
    font-variant-numeric: tabular-nums;
  }
  .thin {
    height: 4px;
    margin-top: 0.6rem;
    border-radius: 99px;
    background: color-mix(in srgb, var(--risu-theme-textcolor) 8%, transparent);
    overflow: hidden;
  }
  .thin i {
    display: block;
    height: 100%;
    background: linear-gradient(90deg, #22c8c6, var(--risu-theme-primary-500));
    transition: width 0.3s;
  }
  .thin i.pulse {
    animation: pulse 1.4s ease-in-out infinite;
  }
  .conflict {
    display: grid;
    gap: 0.75rem;
    border: 1px solid color-mix(in srgb, var(--risu-theme-danger-400) 45%, transparent);
    border-left-width: 3px;
    border-radius: 0.5rem;
    padding: 0.85rem 1rem;
    background: color-mix(in srgb, var(--risu-theme-danger-400) 6%, transparent);
  }
  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .status-dot,
    .thin i.pulse {
      animation: none;
    }
  }
</style>
