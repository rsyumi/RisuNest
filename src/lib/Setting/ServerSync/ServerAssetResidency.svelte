<script lang="ts">
  import { onMount } from "svelte";
  import { language } from "src/lang";
  import { formatRisuNestStorageBytes as bytes } from "src/ts/storage/risuNestStorageDashboard";
  import { serverSyncError } from "src/ts/storage/sync/serverSync";
  import {
    getAssetResidencyStatus,
    setAssetResidencyPolicy,
    evictLocalAssets,
    cancelAssetResidencyOperation,
    type AssetResidencyStatus,
  } from "src/ts/storage/sync/serverAssetResidency";
  let status = $state<AssetResidencyStatus>();
  let busy = $state(false);
  let error = $state("");
  let freed = $state(0);
  const text = $derived(language.risuNest.serverSync.residency);
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

<section
  class="grid min-w-0 gap-3 rounded border border-darkborderc p-4 text-textcolor"
  aria-label={text.title}
  aria-busy={busy}
>
  <h3 class="font-bold">{text.title}</h3>
  <p class="text-sm text-textcolor2">{text.description}</p>
  <div class="flex flex-wrap gap-2">
    <button
      class={button}
      disabled={busy}
      aria-pressed={status?.policy === "full"}
      onclick={() => run(() => setAssetResidencyPolicy("full"))}
      >{text.full}</button
    >
    <button
      class={button}
      disabled={busy}
      aria-pressed={status?.policy === "remote"}
      onclick={() => run(() => setAssetResidencyPolicy("remote"))}
      >{text.remote}</button
    >
  </div>
  {#if status}
    <p class="text-sm">
      {text.selected}: {status.policy === "full" ? text.full : text.remote}
    </p>
    <dl class="grid grid-cols-[1fr_auto] gap-2 text-sm">
      <dt>{text.local}</dt>
      <dd>{bytes(status.localBytes)}</dd>
      <dt>{text.remoteOnly}</dt>
      <dd>{bytes(status.remoteBytes)} ({status.remoteObjects})</dd>
    </dl>
    {#if status.remoteObjects === 0 && status.unavailableObjects === 0}<p
        class="text-sm"
      >
        {text.offlineReady}
      </p>{:else}<p class="text-sm text-textcolor2">{text.onlineNeeded}</p>{/if}
    {#if status.unavailableObjects > 0}<p role="alert" class="text-sm">
        {text.unavailable}: {status.unavailableObjects}
      </p>{/if}
  {/if}
  <p class="text-sm text-textcolor2">{text.cleanupNote}</p>
  <div class="flex flex-wrap gap-2">
    <button
      class={button}
      disabled={busy || status?.policy !== "remote"}
      onclick={() => run(evictLocalAssets)}>{text.clean}</button
    >
    <button
      class={button}
      disabled={busy}
      onclick={() => run(getAssetResidencyStatus)}>{text.refresh}</button
    >
    {#if busy}<button
        class={button}
        onclick={() =>
          cancelAssetResidencyOperation().catch((cause) => {
            error = serverSyncError(cause).code;
          })}>{text.cancel}</button
      >{/if}
  </div>
  {#if busy}<p role="status" class="text-sm">{text.working}</p>{/if}
  {#if freed > 0}<p role="status" class="text-sm">
      {text.freed}: {bytes(freed)}
    </p>{/if}
  {#if error}<p role="alert" class="text-sm">
      {language.risuNest.storage.actionFailed} ({error})
    </p>{/if}
</section>
