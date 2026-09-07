<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import { language } from 'src/lang'
    import Check from 'src/lib/UI/GUI/CheckInput.svelte'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import { getDetailedOSLabel } from 'src/ts/platform'
    import { getDeviceSettings, subscribeDeviceSettings, updateDeviceSettings } from 'src/ts/storage/deviceSettings'
    import { androidGenerationNotificationsEnabled } from 'src/ts/androidGenerationKeepAlive'

    let notificationStatus = $state<boolean | null>(null)
    let keepAlive = $state(getDeviceSettings().androidKeepAliveDuringGeneration)
    let operatingSystem = $state('')
    let webView = $state('')
    let transfer = $state('')
    const unsubscribe = subscribeDeviceSettings((settings) => {
        keepAlive = settings.androidKeepAliveDuringGeneration
    })

    function refresh(): void {
        notificationStatus = androidGenerationNotificationsEnabled()
        const bridge = window.RisuGenerationKeepAlive
        if (!bridge) return
        try {
            webView = bridge.webViewVersion()
            transfer = bridge.transferMode()
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
        return () => {
            window.removeEventListener('focus', refresh)
            document.removeEventListener('visibilitychange', refreshOnVisible)
        }
    })
    onDestroy(unsubscribe)

    $effect(() => {
        updateDeviceSettings({ androidKeepAliveDuringGeneration: keepAlive })
    })
</script>

<h2 class="mb-2 text-2xl font-bold mt-6">{language.risuNest.platform.title}</h2>
<div class="flex flex-col gap-2 text-textcolor">
    {#if notificationStatus !== null}
        <div class="flex items-center gap-2">
            <span>{language.risuNest.platform.notifications}:</span>
            <span
                role="status"
                aria-live="polite"
                class={`inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-semibold ${notificationStatus
                    ? 'border-success-500 bg-success-500/10 text-textcolor'
                    : 'border-draculared bg-draculared/10 text-textcolor'}`}
            >
                {notificationStatus ? language.risuNest.platform.notificationsOn : language.risuNest.platform.notificationsOff}
            </span>
        </div>
    {/if}
    <Check bind:check={keepAlive} name={language.risuNest.platform.keepAlive} />
    <span class="text-textcolor2 text-sm">{language.risuNest.platform.keepAliveHelp}</span>
    {#if notificationStatus === false}
        <span class="text-draculared text-sm" role="alert">{language.risuNest.platform.keepAliveNeedsNotifications}</span>
    {/if}
    <div class="flex flex-wrap gap-2">
        <Button onclick={openNotificationSettings}>{language.risuNest.platform.openSettings}</Button>
    </div>
    {#if operatingSystem || webView || transfer}
        <div data-platform-info class="mt-2 flex flex-col gap-1 text-sm text-textcolor2">
            {#if operatingSystem}
                <span>{language.risuNest.platform.operatingSystem}: {operatingSystem}</span>
            {/if}
            {#if webView}
                <span>{language.risuNest.platform.webView}: {webView}</span>
            {/if}
            {#if transfer}
                <span>{language.risuNest.platform.transferMode}: {transfer}</span>
            {/if}
        </div>
    {/if}
</div>
