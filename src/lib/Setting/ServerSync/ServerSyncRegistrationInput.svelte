<script lang="ts">
  import { onMount, tick } from "svelte";
  import { language } from "src/lang";
  import { isTauriAndroid } from "src/ts/platform";
  import { modalNavigation } from "src/ts/ui/modalNavigation";
  import type { ServerConfig } from "src/ts/storage/sync/serverSync";
  import {
    parseServerRegistration,
    RegistrationError,
  } from "src/ts/storage/sync/serverSyncRegistration";
  import { serverRegistrationInbox } from "src/ts/storage/sync/serverSyncRegistrationInbox";
  import { createServerQrScanner } from "src/ts/storage/sync/serverSyncQr";
  let {
    available = true,
    busy = false,
    onRegistration,
  }: {
    available?: boolean;
    busy?: boolean;
    onRegistration: (config: ServerConfig) => void;
  } = $props();
  let code = $state("");
  let message = $state("");
  let scanning = $state(false);
  let camera = $state(false);
  let mounted = true;
  const scanner = createServerQrScanner();
  const text = $derived(language.risuNest.serverSync);
  function accept(config: ServerConfig) {
    if (!available || busy) {
      message = text.registrationBlocked;
      return;
    }
    message = "";
    code = "";
    onRegistration(config);
  }
  function read() {
    if (busy || !available) return;
    try {
      accept(parseServerRegistration(code.trim()));
    } catch {
      code = "";
      message = text.registrationInvalid;
    }
  }
  async function scan() {
    if (busy || !available || scanning) return;
    scanning = true;
    message = "";
    try {
      const config = await scanner.scan(() => {
        if (mounted) {
          camera = true;
          document.documentElement.classList.add("risunest-qr-scanning");
        }
      });
      if (mounted) accept(config);
    } catch (error) {
      if (
        mounted &&
        !(
          error instanceof RegistrationError &&
          error.code === "qr-scan-cancelled"
        )
      ) {
        message =
          error instanceof RegistrationError &&
          error.code === "qr-camera-permission-denied"
            ? text.cameraDenied
            : error instanceof RegistrationError &&
                error.code.startsWith("invalid-")
              ? text.registrationInvalid
              : text.cameraUnavailable;
      }
    } finally {
      document.documentElement.classList.remove("risunest-qr-scanning");
      camera = false;
      scanning = false;
    }
  }
  onMount(() => {
    const unsubscribe = serverRegistrationInbox.changed.subscribe(() => {
      void tick().then(() => {
        if (!mounted) return;
        const config = serverRegistrationInbox.take();
        if (config) {
          if (!available || busy) {
            serverRegistrationInbox.releaseConsumed();
            message = text.registrationBlocked;
            return;
          }
          accept(config);
        }
      });
    });
    const hidden = () => {
      if (document.hidden && camera) scanner.cancel();
    };
    document.addEventListener("visibilitychange", hidden);
    return () => {
      mounted = false;
      code = "";
      scanner.cancel();
      serverRegistrationInbox.releaseConsumed();
      unsubscribe();
      document.removeEventListener("visibilitychange", hidden);
      document.documentElement.classList.remove("risunest-qr-scanning");
    };
  });
</script>

{#if available}
  <div class="registration border border-darkborderc bg-darkbg">
    <label class="flex flex-col gap-2 font-medium">
      <span>{text.registrationCode}</span>
      <input
        type="password"
        bind:value={code}
        disabled={busy || scanning}
        maxlength="2048"
        autocomplete="new-password"
        autocapitalize="none"
        spellcheck="false"
        class="rounded border border-darkborderc bg-bgcolor p-3 font-mono text-sm"
      />
    </label>
    <p class="text-sm opacity-75">{text.registrationCodeHelp}</p>
    <div class="flex flex-wrap gap-2">
      <button
        type="button"
        disabled={busy || scanning || !code.trim()}
        onclick={read}
        class="rounded border border-darkborderc bg-darkbutton px-4 py-2 hover:bg-selected"
        >{text.readRegistration}</button
      >
      {#if isTauriAndroid}<button
          type="button"
          disabled={busy || scanning}
          onclick={() => void scan()}
          class="rounded border border-darkborderc px-4 py-2 hover:bg-selected"
          >{text.scanRegistration}</button
        >{/if}
    </div>
  </div>
{/if}
{#if message}<p role="status" class="text-sm">{message}</p>{/if}
{#if scanning}
  <div
    class="scanner-overlay"
    class:camera
    role="dialog"
    aria-modal="true"
    aria-label={text.scanRegistration}
    tabindex="-1"
    use:modalNavigation={{ close: () => scanner.cancel() }}
  >
    <div
      class="scanner-instructions bg-darkbg text-textcolor border border-darkborderc"
    >
      <p>{text.scanRegistrationHelp}</p>
      <button
        type="button"
        onclick={() => scanner.cancel()}
        class="rounded border border-darkborderc bg-darkbutton px-5 py-3 hover:bg-selected"
        >{text.cancelScan}</button
      >
    </div>
    {#if camera}<div class="scan-frame" aria-hidden="true"></div>{/if}
  </div>
{/if}

<style>
  .registration {
    display: grid;
    gap: 0.85rem;
    padding: 1rem;
    border-radius: 0.75rem;
  }
  input {
    min-width: 0;
    width: 100%;
  }
  button {
    min-height: 44px;
  }
  button:disabled {
    opacity: 0.45;
  }
  .scanner-overlay {
    position: fixed;
    inset: 0;
    z-index: 2147483000;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: space-between;
    gap: 2rem;
    padding: max(1rem, env(safe-area-inset-top)) 1rem
      max(2rem, env(safe-area-inset-bottom));
    background: var(--risu-theme-bgcolor);
  }
  .scanner-overlay.camera {
    background: transparent;
  }
  .scanner-instructions {
    display: grid;
    gap: 1rem;
    padding: 1rem;
    border-radius: 0.75rem;
    width: min(100%, 28rem);
  }
  .scan-frame {
    width: min(75vw, 55vh);
    aspect-ratio: 1;
    border: 3px solid white;
    border-radius: 1rem;
    margin: auto;
    box-shadow: 0 0 0 2px #0008;
  }
  :global(html.risunest-qr-scanning),
  :global(html.risunest-qr-scanning body) {
    background: transparent !important;
  }
  :global(html.risunest-qr-scanning body *) {
    visibility: hidden;
  }
  :global(html.risunest-qr-scanning .scanner-overlay),
  :global(html.risunest-qr-scanning .scanner-overlay *) {
    visibility: visible;
  }
</style>
