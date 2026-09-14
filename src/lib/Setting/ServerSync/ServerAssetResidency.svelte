<script lang="ts">
  import { onMount } from "svelte";
  import { language } from "src/lang";
  import SegmentedButtons from "../RisuNest/SegmentedButtons.svelte";
  import SettingRow from "../RisuNest/SettingRow.svelte";
  import { formatRisuNestStorageBytes as bytes } from "src/ts/storage/risuNestStorageDashboard";
  import { serverSyncError } from "src/ts/storage/sync/serverSync";
  import {
    getAssetResidencyStatus,
    setAssetResidencyPolicy,
    evictLocalAssets,
    cancelAssetResidencyOperation,
    type AssetResidencyPolicy,
    type AssetResidencyStatus,
  } from "src/ts/storage/sync/serverAssetResidency";
  let { disabled = false }: { disabled?: boolean } = $props();
  let status = $state<AssetResidencyStatus>();
  let busy = $state(false);
  let error = $state("");
  let freed = $state(0);
  const text = $derived(language.risuNest.serverSync.residency);
  const policy = $derived<AssetResidencyPolicy>(status?.policy ?? "full");
  const options = $derived([
    { value: "full" as const, label: text.full },
    { value: "remote" as const, label: text.remote },
  ]);
  const button =
    "rounded border border-darkborderc px-3 py-2 text-sm hover:bg-selected disabled:opacity-40 disabled:cursor-not-allowed";
  async function run(action: () => Promise<AssetResidencyStatus>) {
    if (busy) return;
    busy = true;
    error = "";
    freed = 0;
    try {
      status = await action();
      freed = status.evictedBytes;
    } catch (cause) {
      error = serverSyncError(cause).code;
      try {
        status = await getAssetResidencyStatus();
      } catch {
        /* Keep the last known counts. */
      }
    } finally {
      busy = false;
    }
  }
  onMount(() => {
    void run(getAssetResidencyStatus);
  });
</script>

<SettingRow label={text.title} help={text.description} aria-busy={busy}>
  {#snippet below()}
    {#if status}
      <div class="mt-2 flex flex-wrap gap-x-3.5 gap-y-1 text-[12.5px] text-textcolor/70 tabular-nums">
        <span>{text.local} {bytes(status.localBytes)}</span>
        <span>{text.remoteOnly} {bytes(status.remoteBytes)} ({status.remoteObjects.toLocaleString()})</span>
        <span
          >{status.remoteObjects === 0 && status.unavailableObjects === 0
            ? text.offlineReady
            : text.onlineNeeded}</span
        >
      </div>
      {#if status.unavailableObjects > 0}<p role="alert" class="mt-1 text-sm">
          {text.unavailable}: {status.unavailableObjects.toLocaleString()}
        </p>{/if}
      {#if status.policy === "remote"}<p class="mt-1 text-[12.5px] text-textcolor2">
          {text.cleanupNote}
        </p>{/if}
    {/if}
    {#if busy}<p role="status" class="mt-1 text-sm">{text.working}</p>{/if}
    {#if freed > 0}<p role="status" class="mt-1 text-sm">
        {text.freed}: {bytes(freed)}
      </p>{/if}
    {#if error}<p role="alert" class="mt-1 text-sm">
        {language.risuNest.storage.actionFailed} ({error})
      </p>{/if}
  {/snippet}
  <SegmentedButtons
    value={policy}
    {options}
    label={text.title}
    role="radiogroup"
    onchange={(next) => run(() => setAssetResidencyPolicy(next))}
  />
  <button
    type="button"
    class={button}
    disabled={busy || disabled || policy !== "remote"}
    onclick={() => run(evictLocalAssets)}>{text.clean}</button
  >
  {#if busy}<button
      type="button"
      class={button}
      onclick={() =>
        cancelAssetResidencyOperation().catch((cause) => {
          error = serverSyncError(cause).code;
        })}>{text.cancel}</button
    >{/if}
</SettingRow>
