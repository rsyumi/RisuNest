<script lang="ts">
  import { onMount } from "svelte";
  import { language } from "src/lang";
  import { alertError } from "src/ts/alert";
  import { getDetailedOSLabel } from "src/ts/platform";
  import { getIOSNativeState, openIOSSettings } from "src/ts/iosNative";
  import SettingGroup from "../RisuNest/SettingGroup.svelte";
  import SettingRow from "../RisuNest/SettingRow.svelte";
  import SettingButton from "../RisuNest/SettingButton.svelte";
  import WebViewRefreshButton from "../RisuNest/WebViewRefreshButton.svelte";

  let notifications = $state<boolean | null>(null);
  let os = $state("");
  async function refresh() {
    try {
      notifications = (await getIOSNativeState()).notifications;
      os = await getDetailedOSLabel();
    } catch {
      notifications = null;
    }
  }
  onMount(() => {
    void refresh();
    window.addEventListener("focus", refresh);
    window.addEventListener("risunest-ios-lifecycle", refresh);
    return () => {
      window.removeEventListener("focus", refresh);
      window.removeEventListener("risunest-ios-lifecycle", refresh);
    };
  });
  async function settings() {
    try {
      if (!(await openIOSSettings()).opened)
        throw new Error(language.permissionDenied);
    } catch (error) {
      alertError(String(error));
    }
  }
</script>

<SettingGroup id="risunest-platform" title={language.risuNest.platform.title}>
  <SettingRow
    label={language.risuNest.platform.notifications}
    help={language.risuNest.platform.iosNotificationsHelp}
  >
    {#if notifications !== null}
      <span
        role="status"
        aria-live="polite"
        class={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs font-semibold text-textcolor ${notifications
          ? "border-success-500 bg-success-500/10"
          : "border-draculared bg-draculared/10"}`}
        ><span
          class="h-2 w-2 rounded-full {notifications
            ? 'bg-success-500'
            : 'bg-draculared'}"
          aria-hidden="true"
        ></span>{notifications
          ? language.risuNest.platform.notificationsOn
          : language.risuNest.platform.notificationsOff}</span
      >
    {/if}
    <SettingButton variant="secondary" onclick={settings}
      >{language.risuNest.platform.openSettings}</SettingButton
    >
  </SettingRow>
  <SettingRow
    label={language.risuNest.platform.iosBackgroundLabel}
    help={language.risuNest.platform.iosBackgroundHelp}
  />
  {#if os}
    <dl
      data-platform-info
      class="grid grid-cols-[auto_1fr] gap-x-5 gap-y-1 px-4 py-3 text-sm"
    >
      <dt class="text-subtext">
        {language.risuNest.platform.operatingSystem}
      </dt>
      <dd>{os}</dd>
    </dl>
  {/if}
  <WebViewRefreshButton />
</SettingGroup>
