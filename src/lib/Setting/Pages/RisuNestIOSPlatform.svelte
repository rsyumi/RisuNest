<script lang="ts">
  import { onMount } from "svelte";
  import { language } from "src/lang";
  import { alertError } from "src/ts/alert";
  import { getDetailedOSLabel } from "src/ts/platform";
  import { getIOSNativeState, openIOSSettings } from "src/ts/iosNative";
  import Button from "src/lib/UI/GUI/Button.svelte";
  import SettingGroup from "../RisuNest/SettingGroup.svelte";
  import SettingRow from "../RisuNest/SettingRow.svelte";

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
      <span role="status" class="text-sm text-textcolor2"
        >{notifications
          ? language.risuNest.platform.notificationsOn
          : language.risuNest.platform.notificationsOff}</span
      >
    {/if}
    <Button size="sm" styled="outlined" onclick={settings}
      >{language.risuNest.platform.openSettings}</Button
    >
  </SettingRow>
  <SettingRow
    label={language.risuNest.platform.keepAlive}
    help={language.risuNest.platform.iosBackgroundHelp}
  />
  <SettingRow label={language.risuNest.platform.operatingSystem}
    ><span class="text-sm text-textcolor2">{os}</span></SettingRow
  >
</SettingGroup>
