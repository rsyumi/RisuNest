<script lang="ts">
    import { onMount } from 'svelte'
    import { language } from 'src/lang'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SettingToggle from '../RisuNest/SettingToggle.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import {
        readLocalDataParticipation,
        setLocalDataParticipating,
        type LocalDataSection,
    } from 'src/ts/storage/localDataSections'
    import {
        readLocalDataRemoteState,
        type LocalDataRemoteState,
    } from 'src/ts/storage/localDataRemotes'

    const strings = language.risuNest.localData
    const rows: { section: LocalDataSection; label: string; help: string }[] = [
        { section: 'hypa', label: strings.hypaTitle, help: strings.hypaDescription },
        { section: 'local-plugins', label: strings.pluginTitle, help: strings.pluginDescription },
    ]

    let chosen = $state<Record<LocalDataSection, boolean>>({ hypa: false, 'local-plugins': false })
    let loaded = $state(false)
    let remotes = $state<LocalDataRemoteState>('unknown')
    /** The section waiting for the user to confirm turning it on. */
    let confirming = $state<LocalDataSection | null>(null)
    let busy = $state(false)

    onMount(() => {
        void load()
        void readLocalDataRemoteState().then((state) => { remotes = state })
    })

    async function load(): Promise<void> {
        try {
            for (const row of await readLocalDataParticipation()) {
                chosen[row.section] = row.participating
            }
            loaded = true
        } catch {
            loaded = false
        }
    }

    /**
     * Turning a section on hands the remote this device's values, so it asks
     * first. Turning one off only stops the exchange and needs no answer.
     */
    function change(section: LocalDataSection, included: boolean): void {
        if (included) {
            confirming = section
            return
        }
        void apply(section, false)
    }

    async function apply(section: LocalDataSection, participating: boolean): Promise<void> {
        confirming = null
        busy = true
        try {
            await setLocalDataParticipating(section, participating)
            chosen[section] = participating
        } catch {
            await load()
        } finally {
            busy = false
        }
    }

    function cancelEnable(): void {
        if (confirming) chosen[confirming] = false
        confirming = null
    }
</script>

<SettingGroup id="risunest-local-data" title={strings.title} description={strings.description}>
    {#if remotes === 'none'}
        <p class="px-4 py-3 text-[13px] leading-normal text-textcolor2">{strings.notConnected}</p>
    {/if}
    {#each rows as row (row.section)}
        <SettingRow label={row.label} help={row.help} labelFor={`local-data-${row.section}`}>
            <SettingToggle
                id={`local-data-${row.section}`}
                label={strings.includeInSync}
                showLabel
                disabled={!loaded || busy}
                bind:checked={chosen[row.section]}
                onchange={(included) => change(row.section, included)}
            />
        </SettingRow>
    {/each}
</SettingGroup>

{#if confirming}
    {@const section = confirming}
    <div class="fixed inset-0 z-[1300] flex items-center justify-center bg-black/65 p-4">
        <div
            role="dialog"
            aria-modal="true"
            aria-labelledby="local-data-enable-title"
            data-local-data-enable
            class="flex w-full max-w-md flex-col gap-3 rounded-xl border border-darkborderc bg-darkbg p-5 text-textcolor"
        >
            <h3 id="local-data-enable-title" class="text-lg font-bold">{strings.enableTitle}</h3>
            <p class="text-sm text-textcolor2">{strings.enableBody}</p>
            {#if section === 'local-plugins'}
                <p class="text-sm text-textcolor2">{strings.enableBodyPlugin}</p>
            {/if}
            <div class="flex flex-wrap justify-end gap-2">
                <SettingButton variant="secondary" disabled={busy} onclick={cancelEnable}>{language.cancel}</SettingButton>
                <SettingButton disabled={busy} onclick={() => void apply(section, true)}>{strings.enableConfirm}</SettingButton>
            </div>
        </div>
    </div>
{/if}
