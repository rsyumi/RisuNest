<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { language } from "../../lang";
  import type { DeviceSectionChoice } from "../../ts/storage/deviceBackup/selection";
  import type { NativePortableSelection } from "../../ts/storage/nativeFileJobs";

  let {
    mode,
    choices,
    libraryIncluded,
    repairRequired = false,
    firstRun = false,
    onDone,
    onError,
  }: {
    mode: "export" | "restore";
    choices: DeviceSectionChoice[];
    libraryIncluded: boolean;
    repairRequired?: boolean;
    /** Nothing on the device is at stake, so the help describes an import, not an overwrite. */
    firstRun?: boolean;
    onDone: (selection: NativePortableSelection | null) => void;
    onError: (error: unknown) => void;
  } = $props();
  const text = language.portableBackup;
  let dialog: HTMLDialogElement;
  let finished = false;
  let library = $state(untrack(() => libraryIncluded && !repairRequired));
  let sections = $state(
    untrack(() => choices.map((choice) => ({ ...choice }))),
  );
  const selectedCount = $derived(
    Number(library) + sections.filter((choice) => choice.selected).length,
  );
  const titleId = `portable-selection-${crypto.randomUUID()}`;

  function finish(selection: NativePortableSelection | null) {
    if (finished) return;
    finished = true;
    dialog.close();
    onDone(selection);
  }
  onMount(() => {
    try {
      dialog.showModal();
    } catch (error) {
      onError(error);
    }
    return () => {
      if (dialog.open) dialog.close();
    };
  });
</script>

<dialog
  bind:this={dialog}
  aria-labelledby={titleId}
  aria-describedby={`${titleId}-help`}
  class="m-auto w-[min(34rem,calc(100%-2rem))] max-h-[calc(100dvh-2rem)] overflow-hidden rounded-xl border border-darkborderc bg-bgcolor p-0 text-textcolor shadow-xl backdrop:bg-black/60"
  oncancel={(event) => {
    event.preventDefault();
    finish(null);
  }}
  onclose={() => finish(null)}
>
  <form
    class="flex max-h-[calc(100dvh-2rem)] flex-col"
    onsubmit={(event) => {
      event.preventDefault();
      if (!selectedCount) return;
      finish({
        library: libraryIncluded && !repairRequired && library,
        deviceSections: sections
          .filter((choice) => choice.included && choice.selected)
          .map((choice) => choice.sectionId),
      });
    }}
  >
    <header class="border-b border-darkborderc px-5 pt-5 pb-4">
      <p class="mb-1 text-xs font-medium tracking-wide text-textcolor2">
        {text.title} · .risunest
      </p>
      <h2 id={titleId} class="text-xl font-semibold">
        {mode === "export" ? text.chooseExport : text.chooseRestore}
      </h2>
      <p
        id={`${titleId}-help`}
        class="mt-2 text-sm leading-relaxed text-textcolor2"
      >
        {mode === "export"
          ? text.helpExport
          : firstRun
            ? text.helpRestoreFirstRun
            : text.helpRestore}
      </p>
    </header>
    <div class="overflow-y-auto px-5 py-4">
      {#if repairRequired}
        <p
          class="mb-4 rounded-lg border border-borderc bg-darkbutton p-3 text-sm"
          role="note"
        >
          {text.repair}
        </p>
      {/if}
      {#if libraryIncluded}
        <label
          class="mb-4 flex min-h-12 cursor-pointer items-center gap-3 rounded-lg border border-darkborderc bg-darkbutton px-3 py-2 has-disabled:cursor-default has-disabled:opacity-50"
        >
          <input
            type="checkbox"
            bind:checked={library}
            disabled={repairRequired}
            class="h-4 w-4 shrink-0 accent-[var(--risu-theme-borderc)]"
          />
          <span class="font-medium">{text.library}</span>
        </label>
      {/if}
      {#if sections.some((choice) => choice.sectionId !== "device-settings")}
        <fieldset class="min-w-0">
          <legend
            class="mb-2 text-xs font-medium tracking-wide text-textcolor2"
          >
            {text.device}
          </legend>
          <div
            class="divide-y divide-darkborderc rounded-lg border border-darkborderc"
          >
            {#each sections.filter((choice) => choice.sectionId !== "device-settings") as choice (choice.sectionId)}
              <label
                class="flex min-h-12 cursor-pointer items-center gap-3 px-3 py-2 transition-colors hover:bg-darkbutton"
              >
                <input
                  type="checkbox"
                  bind:checked={choice.selected}
                  class="h-4 w-4 shrink-0 accent-[var(--risu-theme-borderc)]"
                />
                <span class="min-w-0 break-words text-sm">{choice.label}</span>
              </label>
            {/each}
          </div>
        </fieldset>
      {/if}
      {#each sections.filter((choice) => choice.sectionId === "device-settings") as choice (choice.sectionId)}
        <label
          class="mt-4 flex min-h-12 cursor-pointer items-center gap-3 rounded-lg border border-darkborderc px-3 py-2 hover:bg-darkbutton"
        >
          <input
            type="checkbox"
            bind:checked={choice.selected}
            class="h-4 w-4 shrink-0 accent-[var(--risu-theme-borderc)]"
          />
          <span class="text-sm">{text.settings}</span>
        </label>
      {/each}
    </div>
    <footer
      class="flex shrink-0 justify-end gap-2 border-t border-darkborderc bg-darkbg px-5 py-4"
    >
      <button
        type="button"
        onclick={() => finish(null)}
        class="min-h-10 rounded-lg px-4 text-sm hover:bg-darkbutton focus-visible:outline-2 focus-visible:outline-borderc"
      >
        {text.cancel}
      </button>
      <button
        type="submit"
        disabled={!selectedCount}
        class="min-h-10 rounded-lg border border-borderc bg-darkbutton px-5 text-sm font-medium hover:bg-selected focus-visible:outline-2 focus-visible:outline-borderc disabled:cursor-not-allowed disabled:opacity-40"
      >
        {text.continue}
      </button>
    </footer>
  </form>
</dialog>
