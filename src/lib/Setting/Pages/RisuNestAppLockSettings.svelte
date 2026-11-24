<script lang="ts">
    import { onMount, tick } from 'svelte'
    import { language } from 'src/lang'
    import { appLock, type AppLockTiming } from 'src/ts/appLock/appLock.svelte'
    import { getFallbackOSLabel, isTauriIOS } from 'src/ts/platform'
    import SelectInput from '../../UI/GUI/SelectInput.svelte'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import SettingNotice from '../RisuNest/SettingNotice.svelte'

    const lockState = $derived(appLock.state)
    const record = $derived(lockState.record)
    const text = $derived(language.risuNest.appLock)
    let enabled = $state(false)
    let native = $state(false)
    let preview = $state(false)
    let timing = $state<string>('launch')
    let pending = $state(false)
    let errors = $state({ enable: '', timing: '', native: '', preview: '' })
    const disabled = $derived(pending || lockState.busy || lockState.nativeBusy)
    const method = $derived(lockState.availability.status === 'available' ? lockState.availability.method : 'device')
    const nativeHelp = $derived(method === 'windowsHello' ? text.nativeHelpWindowsHello
        : method === 'faceId' ? text.nativeHelpFaceId : method === 'touchId' ? text.nativeHelpTouchId : text.nativeHelp)
    const timingHelp = $derived(record.timing === 'launch' ? text.timingLaunchHelp
        : record.timing === 'immediate' ? text.timingImmediateHelp
        : text.timingAfterHelp.replace('{0}', record.timing.slice(6, -1)))

    $effect(() => {
        enabled = record.enabled
        native = record.nativeUnlock
        preview = record.hideRecentsPreview
        timing = record.timing
    })
    onMount(() => { void appLock.refreshCapabilities() })

    async function restoreControls(): Promise<void> {
        await tick()
        enabled = record.enabled
        native = record.nativeUnlock
        preview = record.hideRecentsPreview
        timing = record.timing
    }

    async function changeEnabled(value: boolean): Promise<void> {
        enabled = value
        errors.enable = ''
        if (value) {
            appLock.beginSetup()
            await restoreControls()
            return
        }
        pending = true
        try {
            if (!await appLock.setEnabled(false)) errors.enable = language.risuNest.localData.applyFailed
        } catch { errors.enable = language.risuNest.localData.applyFailed }
        finally { await restoreControls(); pending = false }
    }

    async function changeTiming(event: Event & { currentTarget: HTMLSelectElement }): Promise<void> {
        const value = event.currentTarget.value as AppLockTiming
        timing = value
        pending = true
        errors.timing = ''
        try {
            if (!await appLock.setTiming(value)) errors.timing = language.risuNest.localData.applyFailed
        } catch { errors.timing = language.risuNest.localData.applyFailed }
        finally { await restoreControls(); pending = false }
    }

    async function changeNative(value: boolean): Promise<void> {
        native = value
        pending = true
        errors.native = ''
        try {
            const outcome = await appLock.setNativeUnlock(value, text.native)
            if (outcome === 'notEnrolled') {
                errors.native = isTauriIOS ? text.nativeNotEnrolledIos
                    : getFallbackOSLabel().startsWith('Windows') ? text.nativeNotEnrolledWindows : text.nativeNotEnrolled
            } else if (outcome === 'saveFailed') errors.native = language.risuNest.localData.applyFailed
            else if (outcome === 'unavailable' || outcome === 'failed') errors.native = text.nativeUnavailable
        } catch { errors.native = language.risuNest.localData.applyFailed }
        finally { await restoreControls(); pending = false }
    }

    async function changePreview(value: boolean): Promise<void> {
        preview = value
        pending = true
        errors.preview = ''
        try {
            if (!await appLock.setHideRecentsPreview(value)) errors.preview = language.risuNest.localData.applyFailed
        } catch { errors.preview = language.risuNest.localData.applyFailed }
        finally { await restoreControls(); pending = false }
    }
</script>

<SettingGroup id="risunest-app-lock" title={text.title}>
    <SettingRow label={text.enable} help={text.enableHelp} inline>
        {#snippet below()}
            {#if lockState.availability.status !== 'available' || errors.enable}
                <div class="mt-2 grid gap-2">
                    {#if errors.enable}<SettingNotice tone="danger" text={errors.enable} role="alert" />{/if}
                    {#if lockState.availability.status !== 'available'}<SettingNotice tone="info" text={text.noRecovery} />{/if}
                </div>
            {/if}
        {/snippet}
        <SettingToggle id="risunest-app-lock-enabled" label={text.enable} bind:checked={enabled} {disabled} onchange={changeEnabled} />
    </SettingRow>
    {#if record.enabled}
        <SettingRow label={text.pin}>
            <SettingButton {disabled} onclick={() => appLock.beginChange()}>{text.change}</SettingButton>
        </SettingRow>
        <SettingRow label={text.timing} help={timingHelp}>
            {#snippet below()}
                {#if errors.timing}<div class="mt-2"><SettingNotice tone="danger" text={errors.timing} role="alert" /></div>{/if}
            {/snippet}
            <fieldset class="contents" {disabled}>
                <SelectInput bind:value={timing} ariaLabel={text.timing} className="w-full @xl:w-auto @xl:min-w-44" onchange={changeTiming}>
                    <option value="launch">{text.timingLaunch}</option>
                    <option value="immediate">{text.timingImmediate}</option>
                    {#each [1, 5, 15, 30] as minutes}
                        <option value={'after-' + minutes + 'm'}>{text.timingAfter.replace('{0}', String(minutes))}</option>
                    {/each}
                </SelectInput>
            </fieldset>
        </SettingRow>
        {#if lockState.availability.status !== 'unsupported'}
            <SettingRow label={text.native} help={nativeHelp} inline>
                {#snippet below()}
                    {#if errors.native}<div class="mt-2"><SettingNotice tone="danger" text={errors.native} role="alert" /></div>{/if}
                {/snippet}
                <SettingToggle id="risunest-app-lock-native" label={text.native} bind:checked={native} {disabled} onchange={changeNative} />
            </SettingRow>
        {/if}
    {/if}
    {#if lockState.previewSupported}
        <SettingRow label={text.preview} help={text.previewHelp} inline>
            {#snippet below()}
                {#if (record.enabled && !record.hideRecentsPreview) || errors.preview}
                    <div class="mt-2 grid gap-2">
                        {#if record.enabled && !record.hideRecentsPreview}<SettingNotice tone="info" text={text.previewRecommend} />{/if}
                        {#if errors.preview}<SettingNotice tone="danger" text={errors.preview} role="alert" />{/if}
                    </div>
                {/if}
            {/snippet}
            <SettingToggle id="risunest-app-lock-preview" label={text.preview} bind:checked={preview} {disabled} onchange={changePreview} />
        </SettingRow>
    {/if}
</SettingGroup>