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

    onMount(() => {
        refresh()
        void getDetailedOSLabel().then((label) => { operatingSystem = label })
        window.addEventListener('focus', refresh)
        return () => window.removeEventListener('focus', refresh)
    })
    onDestroy(unsubscribe)

    $effect(() => {
        updateDeviceSettings({ androidKeepAliveDuringGeneration: keepAlive })
    })
</script>

<h2 class="mb-2 text-2xl font-bold mt-2">{language.risuNest.platform.title}</h2>
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
    <Button onclick={openNotificationSettings}>{language.risuNest.platform.openSettings}</Button>
    <Check bind:check={keepAlive} name={language.risuNest.platform.keepAlive} />
    <span class="text-textcolor2 text-sm">{language.risuNest.platform.keepAliveHelp}</span>
    {#if notificationStatus === false}
        <span class="text-draculared text-sm" role="alert">{language.risuNest.platform.keepAliveNeedsNotifications}</span>
    {/if}
    <span>{language.risuNest.platform.operatingSystem}: {operatingSystem}</span>
    <span>{language.risuNest.platform.webView}: {webView}</span>
    <span>{language.risuNest.platform.transferMode}: {transfer}</span>
</div>
