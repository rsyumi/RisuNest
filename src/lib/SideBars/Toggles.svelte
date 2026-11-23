<script lang="ts">
    import { untrack } from 'svelte'
    import { getModuleToggles } from "src/ts/process/modules";
    import { DBState, selectedCharID } from "src/ts/stores.svelte";
    import { parseToggleSyntax } from "src/ts/util";
    import { language } from "src/lang";
    import type { PromptItem } from "src/ts/process/prompt";
    import { getCurrentCharacter, getCurrentChat, type character, type Database } from "src/ts/storage/database.svelte";
    import SwitchInput from "../UI/GUI/SwitchInput.svelte";
    import ToggleItems from './ToggleItems.svelte'
    import ModelBind from './ModelBind.svelte'
    import PersonaBind from './PersonaBind.svelte'
    import PresetBind from './PresetBind.svelte'
    import ToggleBind from './ToggleBind.svelte'
    import CustomSideBar from "./CustomSidebar.svelte";
    import { getGlobalChatVar, isLocallyHandledGlobalChatVar } from "src/ts/parser/chatVar.svelte";
    import { applyGlobalToggleValues, applyLocalToggleValues, removeLocalToggleValue, setCharacterMemory, setLocalToggleMode, setToggleValue } from 'src/ts/sidebarToggles'
    import { applyToggleValues, snapshotToggleValues, toggleValueChanged, type ToggleValues } from 'src/ts/toggleBindings'
    import { groupSidebarToggles } from 'src/ts/toggleDefinitions'
    import { PinIcon } from "@lucide/svelte";
    import { doingChat } from 'src/ts/process/generationState'

    interface Props {
        chara?: character
        noContainer?: boolean
        /** The preset whose toggles show; the applied preset unless the settings window's preview passes the one it edits. */
        presetDatabase?: Database
        /** The full-screen text editor's preview: no model, persona or preset controls, and of the toggle binding only its save. */
        editorPreview?: boolean
    }

    let { chara = $bindable(), noContainer, presetDatabase, editorPreview = false }: Props = $props();
    let presetFields = $derived(presetDatabase ?? DBState.db)

    const jailbreakToggleToken = '{{jbtoggled}}'
    const usesJailbreakToggle = (value?: string) =>
        typeof value === 'string' && value.includes(jailbreakToggleToken)
    const templateUsesJailbreakToggle = (template: PromptItem[]) =>
        template.some(item => {
            if (item.type === 'jailbreak') {
                return true
            }
            if ('text' in item && usesJailbreakToggle(item.text)) {
                // plain, jailbreak, cot
                return true
            }
            if ('innerFormat' in item && usesJailbreakToggle(item.innerFormat)) {
                // persona, description, lorebook, postEverything, memory
                return true
            }
            if ('defaultText' in item && usesJailbreakToggle(item.defaultText)) {
                // author note
                return true
            }
            return false
        })

    let hasJailbreakPrompt = $derived.by(() => {
        const template = presetFields.promptTemplate
        if (!template) {
            return (presetFields.jailbreak ?? '').trim().length > 0
        }
        return templateUsesJailbreakToggle(template)
    })

    // let getToggleDisplayName = (toggle: sidebarToggle) => {
    //     if(isLocallyHandledGlobalChatVar(`toggle_${toggle.key}`)){
    //         return toggle.value + ' (📌)'
    //     }
    //     return toggle.value
    // }
    let charToggle = $state((DBState.db?.characters?.[$selectedCharID] as character)?.customModuleToggle)
    $effect(() => {
        const charToggleTemp = (DBState.db?.characters?.[$selectedCharID] as character)?.customModuleToggle
        if(charToggleTemp !== charToggle) {
            charToggle = charToggleTemp
        }
    })

    let groupedToggles = $derived(groupSidebarToggles(parseToggleSyntax(
        presetFields.customPromptTemplateToggle + '\n' +
        getModuleToggles(presetFields) + '\n' +
        charToggle
    )))

    // Values that differ from the chat's toggle binding are tinted so the user can see what the
    // save button would write. Nothing is tinted while binding is temporarily disabled.
    let savedToggles = $derived.by(() => {
        if (DBState.db.disableToggleBinding) return undefined
        const character = DBState.db.characters[$selectedCharID]
        return character?.chats[character.chatPage]?.savedToggleValues
    })
    let savingToggles = $state(false)
    // Unapplied input for a bound chat: edited keys (null removes one) and local overrides to remove.
    // A binding, selection or local-mode change discards it; replacing the underlying objects does not.
    let toggleDraftScope = $derived.by(() => {
        const character = DBState.db.characters[$selectedCharID]
        const chat = character?.chats[character.chatPage]
        return JSON.stringify([
            character?.chaId, chat?.id ?? character?.chatPage, savedToggles !== undefined, !!chat?.useLocallySetGlobalVariables,
        ])
    })
    let toggleDraft = $derived.by(() => {
        void toggleDraftScope
        return { edits: {} as Record<string, string | null>, unpinKeys: [] as string[] }
    })
    let toggleEditor = $derived.by(() => {
        const values = snapshotToggleValues(DBState.db.globalChatVariables)
        for (const [key, value] of Object.entries(toggleDraft.edits)) {
            if (value === null) delete values[key]
            else values[key] = value
        }
        return { values, unpinKeys: toggleDraft.unpinKeys }
    })
    // Input that the applied values already hold, such as after saving, is no longer an edit.
    $effect.pre(() => {
        const applied = snapshotToggleValues(DBState.db.globalChatVariables)
        const character = DBState.db.characters[$selectedCharID]
        const overrides = { ...character?.chats[character.chatPage]?.GLGlobalVariables }
        untrack(() => {
            const edits = Object.entries(toggleDraft.edits).filter(([key, value]) => value !== (applied[key] ?? null))
            const unpinKeys = toggleDraft.unpinKeys.filter((key) => overrides[key] !== undefined)
            if (edits.length !== Object.keys(toggleDraft.edits).length || unpinKeys.length !== toggleDraft.unpinKeys.length) {
                toggleDraft = { edits: Object.fromEntries(edits), unpinKeys }
            }
        })
    })
    const stageToggleValues = (values: ToggleValues, keys: readonly string[]) => {
        const applied = snapshotToggleValues(DBState.db.globalChatVariables)
        const chat = getCurrentChat()
        // Outside local mode a new value replaces the chat's local override, as setToggleValue does.
        const overrides = chat?.useLocallySetGlobalVariables ? undefined : chat?.GLGlobalVariables
        const edits = { ...toggleDraft.edits }
        const unpinKeys = new Set(toggleDraft.unpinKeys)
        for (const key of keys) {
            if ((values[key] ?? null) === (applied[key] ?? null)) delete edits[key]
            else edits[key] = values[key] ?? null
            if (overrides?.[key] !== undefined) unpinKeys.add(key)
        }
        toggleDraft = { edits, unpinKeys: [...unpinKeys] }
    }
    const editToggle = (key: string, value: string) => {
        if (savingToggles) return
        const chat = getCurrentChat()
        if (savedToggles === undefined || chat?.useLocallySetGlobalVariables) {
            void setToggleValue(key, value)
            return
        }
        stageToggleValues({ [key]: value }, [key])
    }
    const applyTogglePreset = async (values: ToggleValues, keys: readonly string[]): Promise<boolean> => {
        if (savingToggles) return false
        if (getCurrentChat()?.useLocallySetGlobalVariables) return applyLocalToggleValues(values, keys)
        if (savedToggles === undefined) return applyGlobalToggleValues(values, keys)
        const next = { ...toggleEditor.values }
        applyToggleValues(next, values, keys)
        stageToggleValues(next, [...keys, ...Object.keys(values)].filter((key) => key.startsWith('toggle_')))
        return true
    }
    const isToggleDirty = (key: string | undefined) =>
        savedToggles !== undefined &&
        (toggleEditor.unpinKeys.includes(`toggle_${key}`)
            || toggleValueChanged(toggleEditor.values[`toggle_${key}`], savedToggles[`toggle_${key}`]))

    // The editor preview keeps the pinned settings, and the toggle binding's save that a bound chat needs to apply edits.
    let customItemsShown = $derived(!!DBState.db.customSidebarItems?.some((item) => !editorPreview || item.type === 'setting'))
    let toggleBindShown = $derived(!editorPreview || savedToggles !== undefined)

    // Switch rows sit in the same list as the select/text rows below, which set their own spacing.
    const switchRow = 'mt-2 px-1'

    const getGlobalChatVarNH = (key: string) => {
        const preview = savedToggles !== undefined && (
            toggleEditor.unpinKeys.includes(key) || !isLocallyHandledGlobalChatVar(key)
        )
        const value = preview ? (toggleEditor.values[key] ?? 'null') : getGlobalChatVar(key)
        if (value === 'null') {
            return ''
        }
        return value
    }
    const localEditBlocked = (key: string) => savingToggles || ($doingChat && (
        getCurrentChat()?.useLocallySetGlobalVariables
        || getCurrentChat()?.GLGlobalVariables?.[key] !== undefined
    ))
</script>

{#snippet sidebarToggles(reverse: boolean)}
    <ToggleItems
        items={groupedToggles}
        {reverse}
        value={getGlobalChatVarNH}
        onEdit={editToggle}
        blocked={localEditBlocked}
        dirty={isToggleDirty}
        pinned={isLocallyHandledGlobalChatVar}
        pinDisabled={$doingChat || savingToggles}
        onUnpin={(name) => {
            void removeLocalToggleValue(name)
        }}
    />
{/snippet}

{#if customItemsShown || toggleBindShown}
    <div class="flex flex-col gap-4 w-full mt-3">
        {#if !editorPreview}
            {#if !DBState.db.customSidebarItems?.some((item) => item.type === 'model')}<ModelBind />{/if}
            {#if !DBState.db.customSidebarItems?.some((item) => item.type === 'persona')}<PersonaBind />{/if}
            <PresetBind />
        {/if}
        {#if customItemsShown}<CustomSideBar settingsOnly={editorPreview} />{/if}
        {#if toggleBindShown}
            <ToggleBind values={toggleEditor.values} unpinKeys={toggleEditor.unpinKeys} onApplyPreset={applyTogglePreset} saveOnly={editorPreview} bind:saving={savingToggles} />
        {/if}
    </div>
{/if}
{#if !noContainer && groupedToggles.length > 4}
    <div class="h-48 border-darkborderc p-2 border rounded-sm flex flex-col items-start mt-2 overflow-y-auto">
        {#if hasJailbreakPrompt}
            <SwitchInput className={switchRow} bind:check={DBState.db.jailbreakToggle} name={language.jailbreakToggle} />
        {/if}

        {@render sidebarToggles(true)}
        {#if chara && (DBState.db.supaModelType !== 'none' || DBState.db.hanuraiEnable || DBState.db.hypaV3)}
            <SwitchInput
                className={switchRow}
                check={chara.supaMemory}
                onChange={(checked) => void setCharacterMemory(chara, checked)}
                name={DBState.db.hypaV3
                    ? language.ToggleHypaMemory
                    : DBState.db.hanuraiEnable
                      ? language.hanuraiMemory
                      : DBState.db.hypaMemory
                        ? language.ToggleHypaMemory
                        : language.ToggleSuperMemory}
            />
        {/if}
    </div>
{:else}
    {#if hasJailbreakPrompt}
        <SwitchInput className={switchRow} bind:check={DBState.db.jailbreakToggle} name={language.jailbreakToggle} />
    {/if}
    {@render sidebarToggles(false)}
    {#if chara && (DBState.db.supaModelType !== 'none' || DBState.db.hanuraiEnable || DBState.db.hypaV3)}
        <SwitchInput
            className={switchRow}
            check={chara.supaMemory}
            onChange={(checked) => void setCharacterMemory(chara, checked)}
            name={DBState.db.hypaV3
                ? language.ToggleHypaMemory
                : DBState.db.hanuraiEnable
                  ? language.hanuraiMemory
                  : DBState.db.hypaMemory
                    ? language.ToggleHypaMemory
                    : language.ToggleSuperMemory}
        />
    {/if}

    {#if chara}
        <SwitchInput
            className={switchRow}
            check={getCurrentChat()?.useLocallySetGlobalVariables}
            disabled={$doingChat || savingToggles}
            name={language.localToggles}
            onChange={(checked) => void setLocalToggleMode(checked)}
        />
    {/if}
{/if}
