<script lang="ts">
    // The shell that stands in for the app when the last start did not finish. It calls native
    // commands only: no library, plugin, module or sync state has been initialised behind it.
    import { invoke } from '@tauri-apps/api/core'
    import { language } from 'src/lang'
    import SettingButton from 'src/lib/Setting/RisuNest/SettingButton.svelte'
    import Check from 'src/lib/UI/GUI/CheckInput.svelte'
    import RisuNestDataHealth from 'src/lib/Setting/Pages/RisuNestDataHealth.svelte'
    import { isTauri } from 'src/ts/platform'
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

    const exportSource = async () => {
        exportMessage = ''
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
        }
    }

    // Nothing has opened the store, because opening it is part of the start this shell replaced.
    // The data check needs it and nothing else here does, so it opens it and stops there.
    let opened: Promise<void> | null = null
    const openStore = (): Promise<void> => {
        opened ??= invoke<unknown>('pds_open').then(() => undefined)
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

    let summary = $derived.by(() => {
        const lines = [
            strings.failures.replace(
                '{0}',
                (recovery.decision?.consecutiveFailures ?? 0).toLocaleString(),
            ),
            recovery.trail.stage
                ? strings.stage.replace('{0}', recovery.trail.stage)
                : strings.stageUnknown,
        ]
        if (recovery.trail.suspect)
            lines.push(strings.suspect.replace('{0}', recovery.trail.suspect))
        const previous = recovery.decision?.previous
        if (previous)
            lines.push(
                strings.lastAttempt
                    .replace('{0}', new Date(previous.startedAt).toLocaleString())
                    .replace('{1}', previous.appVersion),
            )
        return lines
    })
</script>

<div data-recovery-shell class="h-full w-full overflow-y-auto bg-darkbg text-textcolor">
    <div class="mx-auto flex w-full max-w-3xl flex-col gap-4 p-4 sm:p-6">
        <header>
            <h1 class="text-2xl font-bold">{strings.title}</h1>
            <p class="mt-1 text-sm text-textcolor2">
                {recovery.mode === 'choose' ? strings.chooseHelp : strings.recoveryHelp}
            </p>
        </header>

        <section data-recovery-summary class="rounded-lg border border-darkborderc bg-bgcolor p-4">
            <h2 class="text-lg font-bold">{strings.summaryTitle}</h2>
            <ul class="mt-1.5 text-sm text-textcolor2">
                {#each summary as line (line)}
                    <li>{line}</li>
                {/each}
            </ul>
        </section>

        <section data-recovery-exclusions class="rounded-lg border border-darkborderc bg-bgcolor p-4">
            <h2 class="text-lg font-bold">{strings.excludeTitle}</h2>
            <p class="mt-1 text-sm text-textcolor2">{strings.excludeHelp}</p>
            <div class="mt-2 flex flex-col gap-1">
                {#each RECOVERY_EXCLUSIONS as exclusion (exclusion)}
                    <Check
                        check={recovery.excluded.includes(exclusion)}
                        margin={false}
                        name={exclusionLabels[exclusion]}
                        onChange={() => toggleExclusion(exclusion)}
                    />
                {/each}
            </div>
        </section>

        <RisuNestDataHealth prepare={openStore} />

        {#if isTauri}
        <section data-recovery-export class="rounded-lg border border-darkborderc bg-bgcolor p-4">
            <h2 class="text-lg font-bold">{strings.exportTitle}</h2>
            <p class="mt-1 mb-2 text-sm text-textcolor2">{strings.exportHelp}</p>
            <SettingButton variant="secondary" onclick={exportSource}>{strings.exportAction}</SettingButton>
            {#if exportMessage}
                <p class="mt-2 text-sm text-textcolor2" role="status">{exportMessage}</p>
            {/if}
        </section>
        {/if}

        <div class="flex flex-wrap gap-2">
            <SettingButton onclick={() => onStart(startNormally())}>{strings.startNormally}</SettingButton>
        </div>
    </div>
</div>
