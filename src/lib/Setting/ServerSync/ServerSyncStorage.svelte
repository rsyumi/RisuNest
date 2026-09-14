<script lang="ts">
  import { onMount } from "svelte";
  import { language } from "src/lang";
  import { alertConfirm } from "src/ts/alert";
  import { formatRisuNestStorageBytes as bytes } from "src/ts/storage/risuNestStorageDashboard";
  import { serverSyncError } from "src/ts/storage/sync/serverSync";
  import {
    getServerSyncBackupInventory,
    getServerSyncCacheUsage,
    cleanupServerSyncCache,
    deleteServerSyncBackup,
    restoreServerSyncBackup,
    type ServerSyncBackupInventory,
    type ServerSyncCacheUsage,
  } from "src/ts/storage/sync/serverSyncProduction";
  /** `backups` lists the conflict backups, `cache` the space they and the
   * temporary files take; `all` shows both under one heading. */
  let {
    onChange,
    section = "all",
  }: { onChange?: () => void; section?: "all" | "backups" | "cache" } =
    $props();
  let inventory = $state<ServerSyncBackupInventory>();
  let cache = $state<ServerSyncCacheUsage>();
  let busy = $state(false);
  let error = $state("");
  const text = $derived(language.risuNest.serverSync);
  const labels = $derived(text.management);
  async function load(older = false): Promise<void> {
    const cursor = older ? (inventory?.next ?? undefined) : undefined;
    const [next, usage] = await Promise.all([
      getServerSyncBackupInventory(cursor),
      getServerSyncCacheUsage(),
    ]);
    inventory = next;
    cache = usage;
  }
  async function action(
    run: () => Promise<unknown>,
    changed = false,
  ): Promise<void> {
    if (busy) return;
    busy = true;
    error = "";
    try {
      await run();
      if (changed) await load();
    } catch (cause) {
      error = serverSyncError(cause).code;
      if (changed) {
        inventory = undefined;
        cache = undefined;
      }
    } finally {
      busy = false;
      if (changed) onChange?.();
    }
  }
  async function remove(id: string): Promise<void> {
    if (await alertConfirm(labels.deleteConfirm))
      await action(() => deleteServerSyncBackup(id), true);
  }
  async function restore(id: string, side: "local" | "remote"): Promise<void> {
    if (await alertConfirm(labels.restoreConfirm))
      await action(() => restoreServerSyncBackup(id, side), true);
  }
  async function clean(): Promise<void> {
    if (await alertConfirm(labels.cleanConfirm))
      await action(cleanupServerSyncCache, true);
  }
  onMount(() => {
    void action(() => load());
  });
  const button =
    "rounded border border-darkborderc px-3 py-2 text-sm hover:bg-selected disabled:opacity-40 disabled:cursor-not-allowed";
</script>

<section
  class="grid min-w-0 gap-3 py-3 text-textcolor"
  aria-label={labels.title}
>
  <div class="flex flex-wrap items-center justify-between gap-2">
    {#if section === "all"}<h3 class="font-bold">{labels.title}</h3>{/if}
    <button
      class="{button} ml-auto"
      disabled={busy}
      onclick={() => action(() => load())}>{labels.refresh}</button
    >
  </div>
  {#if error}<p role="alert" class="text-sm">
      {language.risuNest.storage.actionFailed} ({error})
    </p>{/if}
  {#if inventory && section !== "backups"}
    <dl class="grid grid-cols-[1fr_auto] gap-2 text-sm">
      <dt>{labels.disk}</dt>
      <dd>{bytes(inventory.diskBytes)}</dd>
      <dt>{labels.complete} ({inventory.completeCount})</dt>
      <dd>{bytes(inventory.completeBytes)}</dd>
      <dt>{labels.incomplete} ({inventory.incompleteCount})</dt>
      <dd>{bytes(inventory.incompleteBytes)}</dd>
    </dl>
  {/if}
  {#if inventory && section !== "cache"}
    <p class="text-sm text-textcolor2">{labels.scope}</p>
    {#if inventory.items.length === 0}<p class="text-sm">
        {text.noBackups}
      </p>{/if}
    {#each inventory.items as item (item.id)}
      <div class="grid gap-2 rounded border border-darkborderc p-3">
        <p class="text-sm">
          {new Date(item.createdAt).toLocaleString()} · {bytes(
            item.localBytes + item.remoteBytes,
          )}
        </p>
        <div class="flex flex-wrap gap-2">
          <button
            class={button}
            disabled={busy ||
              !item.recoveryReady ||
              Boolean(item.blockedReason)}
            onclick={() => restore(item.id, "local")}
            >{text.restoreLocalBackup} ({bytes(item.localBytes)})</button
          >
          <button
            class={button}
            disabled={busy ||
              !item.recoveryReady ||
              Boolean(item.blockedReason)}
            onclick={() => restore(item.id, "remote")}
            >{text.restoreRemoteBackup} ({bytes(item.remoteBytes)})</button
          >
          <button
            class={button}
            disabled={busy || !item.deletable}
            onclick={() => remove(item.id)}>{language.remove}</button
          >
        </div>
        {#if item.blockedReason}<p class="text-xs text-textcolor2">
            {item.blockedReason}
          </p>{/if}
      </div>
    {/each}
    {#if inventory.next}<button
        class={button}
        disabled={busy}
        onclick={() => action(() => load(true))}>{labels.more}</button
      >{/if}
  {/if}
  {#if cache && section !== "backups"}
    <div
      class="grid gap-2 text-sm"
      class:border-t={section === "all"}
      class:border-darkborderc={section === "all"}
      class:pt-3={section === "all"}
    >
      <p>{labels.cache}: {bytes(cache.totalBytes)}</p>
      <p>
        {labels.protected}: {bytes(cache.protectedBytes)} · {labels.reclaimable}:
        {bytes(cache.reclaimableBytes)}
      </p>
      <button
        class={button}
        disabled={busy ||
          cache.reclaimableBytes === 0 ||
          Boolean(cache.blockedReason)}
        onclick={clean}>{labels.clean}</button
      >
      {#if cache.blockedReason}<p class="text-textcolor2">
          {cache.blockedReason}
        </p>{/if}
    </div>
  {/if}
</section>
