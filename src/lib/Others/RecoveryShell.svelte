<script lang="ts">
    // The shell that stands in for the app when the last start did not finish. It calls native
    // commands only: no library, plugin, module or sync state has been initialised behind it.
    import { invoke } from '@tauri-apps/api/core'
    import { language } from 'src/lang'
    import SettingButton from 'src/lib/Setting/RisuNest/SettingButton.svelte'
    import SettingGroup from 'src/lib/Setting/RisuNest/SettingGroup.svelte'
    import SettingRow from 'src/lib/Setting/RisuNest/SettingRow.svelte'
    import SettingToggle from 'src/lib/Setting/RisuNest/SettingToggle.svelte'
    import RisuNestDataHealth from 'src/lib/Setting/Pages/RisuNestDataHealth.svelte'
    import LocalDataReset from 'src/lib/Setting/RisuNest/LocalDataReset.svelte'
    import { isTauri } from 'src/ts/platform'
    import type { BootStage } from 'src/ts/bootFailureClassification'
    import { exportOriginalData } from 'src/ts/storage/rawRecoveryExport'
    import {
        RECOVERY_EXCLUSIONS,
        recoveryState,
        startNormally,
        toggleExclusion,
        type RecoveryExclusion,
    } from 'src/ts/storage/recoveryMode.svelte'

    interface Props {
        /** Hands over to the ordinary start with the exclusions this run keeps. */
        onStart: (excluded: readonly RecoveryExclusion[]) => void
    }

    let { onStart }: Props = $props()
    let exportMessage = $state('')
    let exporting = $state(false)

    const exportSource = async () => {
        exportMessage = ''
        exporting = true
        try {
            const result = await exportOriginalData()
            if (!result) return
            exportMessage = result.warningCodes.includes('source-problems')
                ? strings.exportPartial
                : strings.exportComplete
        } catch (error) {
            if (error instanceof DOMException && error.name === 'AbortError') {
                exportMessage = strings.exportCancelled
            } else {
                exportMessage = strings.exportFailed
            }
        } finally {
            exporting = false
        }
    }

    // Nothing has opened the store, because opening it is part of the start this shell replaced.
    // The data check needs it and nothing else here does, so it opens it and stops there.
    let opened: Promise<void> | null = null
    const openStore = (): Promise<void> => {
        opened ??= invoke<unknown>('pds_open').then(() => undefined).catch(error => { opened = null; throw error })
        return opened
    }

    const strings = language.risuNest.recovery
    const recovery = recoveryState()
    const exclusionLabels: Record<RecoveryExclusion, string> = {
        plugins: strings.excludePlugins,
        modules: strings.excludeModules,
        regex: strings.excludeRegex,
        theme: strings.excludeTheme,
        sync: strings.excludeSync,
        autoUpdate: strings.excludeAutoUpdate,
        account: strings.excludeAccount,
    }

    const startup = language.risuNest.startup
    /** The loading text the start showed at each recorded stage. */
    const stageLabels: Partial<Record<BootStage, string>> = {
        'app-data-directories': startup.storage,
        'persistent-storage': startup.storage,
        'plugin-compatibility-data': startup.compatibility,
        'persistent-database': startup.data,
        'format-update': startup.data,
        'account-bootstrap': startup.account,
        'drive-sync': startup.account,
        'account-data': startup.account,
        'service-worker': startup.serviceWorker,
        plugins: startup.plugins,
        'ui-state': startup.ui,
    }

    interface SummaryLine {
        key: string
        /** The message around the recorded value, so only the value breaks. */
        prefix: string
        suffix: string
        value?: string
    }

    const plain = (key: string, text: string): SummaryLine => ({
        key,
        prefix: text,
        suffix: '',
    })
    const around = (key: string, template: string, value: string): SummaryLine => {
        const [prefix, suffix = ''] = template.split('{0}')
        return { key, prefix, suffix, value }
    }

    let summary = $derived.by(() => {
        const stageLabel = recovery.trail.stage ? stageLabels[recovery.trail.stage as BootStage] : undefined
        const lines: SummaryLine[] = [
            plain(
                'failures',
                strings.failures.replace(
                    '{0}',
                    (recovery.decision?.consecutiveFailures ?? 0).toLocaleString(),
                ),
            ),
            stageLabel
                ? around('stage', strings.stage, stageLabel)
                : plain('stage', strings.stageUnknown),
        ]
        if (recovery.trail.suspect)
            lines.push(around('suspect', strings.suspect, recovery.trail.suspect))
        const previous = recovery.decision?.previous
        if (previous)
            lines.push(
                plain(
                    'lastAttempt',
                    strings.lastAttempt
                        .replace('{0}', new Date(previous.startedAt).toLocaleString())
                        .replace('{1}', previous.appVersion),
                ),
            )
        return lines
    })
</script>

<div data-recovery-shell class="h-full w-full overflow-y-auto bg-darkbg text-textcolor">
    <div class="mx-auto flex w-full max-w-3xl flex-col p-4 sm:p-6">
        <header>
            <h1 class="text-2xl font-bold">{strings.title}</h1>
            <p class="mt-1 text-sm text-subtext">
                {recovery.mode === 'choose' ? strings.chooseHelp : strings.recoveryHelp}
            </p>
        </header>

        <SettingGroup title={strings.summaryTitle} panelProps={{ 'data-recovery-summary': '' }}>
            {#each summary as line (line.key)}
                <SettingRow>
                    {#snippet below()}
                        <p class="text-[15px]">{line.prefix}{#if line.value}<span class="break-all">{line.value}</span>{/if}{line.suffix}</p>
                    {/snippet}
                </SettingRow>
            {/each}
        </SettingGroup>

        <SettingGroup
            title={strings.excludeTitle}
            description={strings.excludeHelp}
            panelProps={{ 'data-recovery-exclusions': '' }}
        >
            {#each RECOVERY_EXCLUSIONS as exclusion (exclusion)}
                <SettingRow inline label={exclusionLabels[exclusion]}>
                    <SettingToggle
                        checked={recovery.excluded.includes(exclusion)}
                        label={exclusionLabels[exclusion]}
                        onchange={() => toggleExclusion(exclusion)}
                    />
                </SettingRow>
            {/each}
        </SettingGroup>

        <RisuNestDataHealth prepare={openStore} />

        {#if isTauri}
            <SettingGroup title={strings.exportTitle} panelProps={{ 'data-recovery-export': '' }}>
                <SettingRow help={strings.exportHelp}>
                    {#snippet below()}
                        {#if exportMessage}
                            <p
                                class="mt-1.5 text-sm {exportMessage === strings.exportFailed ? 'text-danger-400' : 'text-subtext'}"
                                role="status"
                            >{exportMessage}</p>
                        {/if}
                    {/snippet}
                    <SettingButton variant="secondary" busy={exporting} onclick={exportSource}>{strings.exportAction}</SettingButton>
                </SettingRow>
            </SettingGroup>
        {/if}

        <div class="mt-7 flex flex-wrap gap-2">
            <SettingButton onclick={() => onStart(startNormally())}>{strings.startNormally}</SettingButton>
        </div>
        <LocalDataReset />
    </div>
</div>
