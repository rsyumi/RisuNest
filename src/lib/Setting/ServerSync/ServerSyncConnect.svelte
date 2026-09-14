<script lang="ts">
  import { language } from "src/lang";
  import SegmentedButtons from "../RisuNest/SegmentedButtons.svelte";
  import ServerSyncRegistrationInput from "./ServerSyncRegistrationInput.svelte";
  import type { AssetResidencyPolicy } from "src/ts/storage/sync/serverAssetResidency";
  import type { ServerConfig } from "src/ts/storage/sync/serverSync";
  import { serverSyncError } from "src/ts/storage/sync/serverSync";
  import type { ServerSyncConnectRequest } from "src/ts/storage/sync/serverSyncConnectFlow";
  import { serverSyncErrorHelp } from "src/ts/storage/sync/serverSyncConnectFlow";
  import {
    validateServerSyncEndpoint,
    validateServerSyncId,
  } from "src/ts/storage/sync/serverSyncConnection";
  import type { ServerSyncNavigation } from "src/ts/storage/sync/serverSyncDeepLink";
  import { serverRegistrationInbox } from "src/ts/storage/sync/serverSyncRegistrationInbox";

  /**
   * Registration code, then the server check. Both the onboarding and the
   * settings place it; the parent runs the connection and reports its error.
   */
  let {
    stage = $bindable("code"),
    initialNavigation,
    available = true,
    busy = false,
    replacing = false,
    error = "",
    tone = "settings",
    onSubmit,
  }: {
    stage?: "code" | "review";
    initialNavigation?: ServerSyncNavigation;
    /** False while another server is bound; a delivered code is then refused. */
    available?: boolean;
    busy?: boolean;
    replacing?: boolean;
    /** The parent's last connection error code. */
    error?: string;
    tone?: "settings" | "onboarding";
    onSubmit: (request: ServerSyncConnectRequest) => void;
  } = $props();
  const text = $derived(language.risuNest.serverSync);
  let config = $state<ServerConfig | undefined>();
  let residency = $state<AssetResidencyPolicy>("full");
  let endpoint = $state("");
  let libraryId = $state("");
  let deviceId = $state("");
  let token = $state("");
  let manualOpen = $state(false);
  let formError = $state("");
  let inputRevision = $state(0);
  let appliedNavigation: ServerSyncNavigation | undefined;
  $effect(() => {
    if (!initialNavigation || appliedNavigation === initialNavigation) return;
    appliedNavigation = initialNavigation;
    if (!available || busy || stage !== "code") return;
    endpoint = initialNavigation.endpoint ?? "";
    libraryId = initialNavigation.libraryId ?? "";
    deviceId = "";
    token = "";
    manualOpen = Boolean(endpoint || libraryId);
  });
  const residencyOptions = $derived([
    { value: "full" as const, label: text.residency.full },
    { value: "remote" as const, label: text.residency.remote },
  ]);
  function accept(next: ServerConfig): void {
    if (!available || busy) return;
    // Native binding is authoritative; the address is only normalized here.
    config = { ...next, endpoint: validateServerSyncEndpoint(next.endpoint) };
    formError = "";
    stage = "review";
  }
  function readManualEntry(): void {
    if (busy) return;
    formError = "";
    try {
      accept({
        endpoint: validateServerSyncEndpoint(endpoint.trim()),
        libraryId: validateServerSyncId(libraryId.trim()),
        deviceId: validateServerSyncId(deviceId.trim()),
        token: token.trim(),
      });
    } catch (cause) {
      formError = serverSyncError(cause).code;
    }
  }
  function discard(): void {
    config = undefined;
    endpoint = "";
    libraryId = "";
    deviceId = "";
    token = "";
    manualOpen = false;
    formError = "";
    serverRegistrationInbox.clear();
    inputRevision++;
    stage = "code";
  }
  function submit(): void {
    if (busy || !config) return;
    onSubmit({
      config: $state.snapshot(config),
      residency,
      ...(replacing ? { replacing: true } : {}),
    });
  }
  const button =
    "rounded border border-darkborderc px-4 py-2 text-sm hover:bg-selected disabled:cursor-not-allowed disabled:opacity-45";
</script>

<div class="connect text-textcolor">
  {#if stage === "code"}
    {#key inputRevision}
      <ServerSyncRegistrationInput {available} {busy} onRegistration={accept} />
    {/key}
    {#if available}
      <details class="manual" bind:open={manualOpen}>
        <summary class="text-sm">{text.manualEntry}</summary>
        <form
          class="fields"
          onsubmit={(event) => {
            event.preventDefault();
            readManualEntry();
          }}
        >
          <p class="text-sm text-textcolor2">{text.credentialsHelp}</p>
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
          <div class="identity">
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
          {#if formError}<p class="text-sm" role="alert">
              {text.credentialsHelp} <span class="opacity-60">({formError})</span>
            </p>{/if}
          <div class="actions">
            <button type="submit" class="{button} bg-darkbutton" disabled={busy}
              >{text.reviewTitle}</button
            >
            <button type="button" class={button} disabled={busy} onclick={discard}
              >{text.discardRegistration}</button
            >
          </div>
        </form>
      </details>
    {/if}
  {:else if config}
    <form
      class="review-form"
      onsubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      {#if tone === "settings"}<p class="text-sm text-textcolor2">
          {text.reviewLead}
        </p>{/if}
      <dl class="review">
        <dt>{text.endpoint}</dt>
        <dd>{config.endpoint}</dd>
        <dt>{text.libraryId}</dt>
        <dd>{config.libraryId}</dd>
        <dt>{text.deviceId}</dt>
        <dd>{config.deviceId}</dd>
        <dt>{text.token}</dt>
        <dd aria-label={text.token}>••••••••</dd>
        {#if config.directory}
          <dt>{text.directoryEnabled}</dt>
          <dd>{config.directory.baseUrl}</dd>
        {/if}
      </dl>
      <div class="policy">
        <b class="text-sm font-semibold">{text.residency.title}</b>
        <small class="text-sm text-textcolor2">{text.residency.description}</small>
        <SegmentedButtons
          bind:value={residency}
          options={residencyOptions}
          label={text.residency.title}
          role="radiogroup"
        />
      </div>
      <div class="actions">
        <button type="submit" class="{button} bg-darkbutton" disabled={busy}
          >{replacing ? text.reregister : text.connect}</button
        >
        <button type="button" class={button} disabled={busy} onclick={discard}
          >{text.otherCode}</button
        >
      </div>
      <p class="text-sm text-textcolor2">{text.connectHint}</p>
    </form>
  {/if}
  {#if error && error !== "cancelled"}
    <p class="text-sm" role="alert">
      {serverSyncErrorHelp(error, text)} <span class="opacity-60">({error})</span>
    </p>
  {/if}
</div>

<style>
  .connect {
    container-type: inline-size;
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    min-width: 0;
    gap: 0.85rem;
    overflow-wrap: anywhere;
  }
  .manual summary {
    cursor: pointer;
    opacity: 0.8;
  }
  .manual summary:hover {
    opacity: 1;
  }
  .fields,
  .review-form,
  .field {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 0.5rem;
  }
  .fields {
    gap: 0.85rem;
    margin-top: 0.75rem;
  }
  .review-form {
    gap: 0.85rem;
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
  .field input:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .identity {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 0.85rem;
  }
  .review {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 0.15rem 1.1rem;
    margin: 0;
    padding: 0.85rem 1rem;
    /* The onboarding's confirmation card tint. */
    border: 1px solid rgba(34, 200, 198, 0.35);
    border-radius: 0.75rem;
    background: rgba(34, 200, 198, 0.08);
    font-size: 0.85rem;
  }
  .review dt {
    font-size: 0.78rem;
    color: var(--risu-theme-textcolor2);
  }
  .review dd {
    margin: 0 0 0.5rem;
    min-width: 0;
    font-weight: 600;
  }
  .review dd:last-child {
    margin-bottom: 0;
  }
  .policy {
    display: grid;
    gap: 0.5rem;
    padding: 0.75rem 0.85rem;
    border: 1px solid var(--risu-theme-darkborderc);
    border-radius: 0.75rem;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem;
  }
  button:focus-visible,
  input:focus-visible {
    outline: 2px solid currentColor;
    outline-offset: 3px;
  }
  @container (min-width: 480px) {
    .identity {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
    .review {
      grid-template-columns: auto minmax(0, 1fr);
      gap: 0.5rem 1.1rem;
    }
    .review dd {
      margin: 0;
    }
  }
</style>
