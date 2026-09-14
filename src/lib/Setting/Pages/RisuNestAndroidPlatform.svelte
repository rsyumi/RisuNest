<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { language } from 'src/lang'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import { getDetailedOSLabel } from 'src/ts/platform'
    import { getDeviceSettings, subscribeDeviceSettings, updateDeviceSettings } from 'src/ts/storage/deviceSettings'
    import { androidGenerationNotificationsEnabled } from 'src/ts/androidGenerationKeepAlive'

    let notificationStatus = $state<boolean | null>(null)
    let keepAlive = $state(getDeviceSettings().androidKeepAliveDuringGeneration)
    let operatingSystem = $state('')
    let webView = $state('')
    const unsubscribe = subscribeDeviceSettings((settings) => {
        keepAlive = settings.androidKeepAliveDuringGeneration
    })

    function refresh(): void {
        notificationStatus = androidGenerationNotificationsEnabled()
        const bridge = window.RisuGenerationKeepAlive
        if (!bridge) return
        try {
            webView = bridge.webViewVersion()
        } catch {
            notificationStatus = null
        }
    }

    function openNotificationSettings(): void {
        try {
            window.RisuGenerationKeepAlive?.openNotificationSettings()
        } catch {
            // The visible state stays unchanged until Android resumes this WebView.
        }
    }

    function refreshOnVisible(): void {
        if (document.visibilityState === 'visible') refresh()
    }

    onMount(() => {
        refresh()
        void getDetailedOSLabel().then((label) => { operatingSystem = label })
        // Returning from the system notification screen restores the WebView through either
        // event depending on the Android version, so both are observed.
        window.addEventListener('focus', refresh)
        document.addEventListener('visibilitychange', refreshOnVisible)
        // Android resume does not always produce browser focus/visibility events.
        window.addEventListener('risunest-android-notifications-changed', refresh)
        return () => {
            window.removeEventListener('focus', refresh)
            document.removeEventListener('visibilitychange', refreshOnVisible)
            window.removeEventListener('risunest-android-notifications-changed', refresh)
        }
    })
    onDestroy(unsubscribe)

    function setKeepAlive(next: boolean): void {
        if (next === keepAlive) return
        keepAlive = next
        updateDeviceSettings({ androidKeepAliveDuringGeneration: next })
    }

</script>

<SettingGroup id="risunest-platform" title={language.risuNest.platform.title}>
    <SettingRow label={language.risuNest.platform.notifications} help={language.risuNest.platform.notificationsHelp}>
        {#if notificationStatus !== null}
            <span
                role="status"
                aria-live="polite"
                class={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs font-semibold text-textcolor ${notificationStatus
                    ? 'border-success-500 bg-success-500/10'
                    : 'border-draculared bg-draculared/10'}`}
            ><span class="h-2 w-2 rounded-full {notificationStatus ? 'bg-success-500' : 'bg-draculared'}" aria-hidden="true"></span>{notificationStatus ? language.risuNest.platform.notificationsOn : language.risuNest.platform.notificationsOff}</span>
        {/if}
        <SettingButton variant="secondary" onclick={openNotificationSettings}>{language.risuNest.platform.openSettings}</SettingButton>
    </SettingRow>
    <SettingRow inline label={language.risuNest.platform.keepAlive} help={language.risuNest.platform.keepAliveHelp}>
        {#snippet below()}
            {#if notificationStatus === false}
                <p class="mt-1 text-sm text-draculared" role="alert">{language.risuNest.platform.keepAliveNeedsNotifications}</p>
            {/if}
        {/snippet}
        <SettingToggle checked={keepAlive} onchange={setKeepAlive} label={language.risuNest.platform.keepAlive} />
    </SettingRow>
    {#if operatingSystem || webView}
        <dl data-platform-info class="grid grid-cols-[auto_1fr] gap-x-5 gap-y-1 px-4 py-3 text-sm">
            {#if operatingSystem}
                <dt class="text-textcolor2">{language.risuNest.platform.operatingSystem}</dt><dd>{operatingSystem}</dd>
            {/if}
            {#if webView}
                <dt class="text-textcolor2">{language.risuNest.platform.webView}</dt><dd>{webView}</dd>
            {/if}
        </dl>
    {/if}
</SettingGroup>
