<script lang="ts">
    import { getModuleToggles } from "src/ts/process/modules";
    import { DBState, selectedCharID } from "src/ts/stores.svelte";
    import { parseToggleSyntax, type sidebarToggle, type sidebarToggleGroup } from "src/ts/util";
    import { language } from "src/lang";
    import type { PromptItem } from "src/ts/process/prompt";
    import { getCurrentCharacter, getCurrentChat, type character, type groupChat } from "src/ts/storage/database.svelte";
    import Accordion from '../UI/Accordion.svelte'
    import SwitchInput from "../UI/GUI/SwitchInput.svelte";
    import SelectInput from "../UI/GUI/SelectInput.svelte";
    import OptionInput from "../UI/GUI/OptionInput.svelte";
    import TextAreaInput from '../UI/GUI/TextAreaInput.svelte'
    import TextInput from "../UI/GUI/TextInput.svelte";
    import ModelBind from './ModelBind.svelte'
    import PersonaBind from './PersonaBind.svelte'
    import ToggleBind from './ToggleBind.svelte'
    import CustomSideBar from "./CustomSidebar.svelte";
    import { getGlobalChatVar, isLocallyHandledGlobalChatVar, removeLocallyHandledGlobalChatVar, setGlobalChatVar } from "src/ts/parser/chatVar.svelte";
    import { toggleValueChanged } from 'src/ts/toggleBindings'
    import { PinIcon } from "@lucide/svelte";

    interface Props {
        chara?: character|groupChat
        noContainer?: boolean
    }

    let { chara = $bindable(), noContainer }: Props = $props();

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
        const template = DBState.db.promptTemplate
        if (!template) {
            return (DBState.db.jailbreak ?? '').trim().length > 0
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

    let groupedToggles = $derived.by(() => {
        const ungrouped = parseToggleSyntax(
            DBState.db.customPromptTemplateToggle + '\n' +
            getModuleToggles() + '\n' +
            charToggle
        )

        let groupOpen = false
        // group toggles together between group ... groupEnd
        return ungrouped.reduce<sidebarToggle[]>((acc, toggle) => {
            if (toggle.type === 'group') {
                groupOpen = true
                acc.push(toggle)
            } else if (toggle.type === 'groupEnd') {
                groupOpen = false
            } else if (groupOpen) {
                (acc.at(-1) as sidebarToggleGroup).children.push(toggle)
            } else {
                acc.push(toggle)
            }
            return acc
        }, [])
    })

    // Values that differ from the chat's toggle binding are tinted so the user can see what the
    // save button would write. Nothing is tinted while binding is temporarily disabled.
    let savedToggles = $derived.by(() => {
        if (DBState.db.disableToggleBinding) return undefined
        const character = DBState.db.characters[$selectedCharID]
        return character?.chats[character.chatPage]?.savedToggleValues
    })
    const isToggleDirty = (key: string | undefined) =>
        savedToggles !== undefined &&
        toggleValueChanged(DBState.db.globalChatVariables[`toggle_${key}`], savedToggles[`toggle_${key}`])
    const dirtyClass = (key: string | undefined) => (isToggleDirty(key) ? 'bg-draculared/15' : '')

    const getGlobalChatVarNH = (key: string) => {
        const value = getGlobalChatVar(key)
        if (value === 'null') {
            return ''
        }
        return value
    }
</script>

{#snippet localToggle(toggle: sidebarToggle)}
    {#if isLocallyHandledGlobalChatVar(`toggle_${toggle.key}`)}
        <button
            onclick={() => {
                removeLocallyHandledGlobalChatVar(`toggle_${toggle.key}`)
            }}
        >
            📌
        </button>
    {/if}
{/snippet}

{#snippet getToggleDisplayName(toggle: sidebarToggle)}
    {toggle.value}{@render localToggle(toggle)}
{/snippet}

{#snippet toggles(items: sidebarToggle[], reverse: boolean = false)}
    {#each items as toggle, index}
        {#if index > 0 && toggle.type !== 'divider' && items[index - 1]?.type !== 'divider' && toggle.type !== 'caption' && items[index - 1]?.type !== 'caption' && !(toggle.type === 'group' && items[index - 1]?.type === 'group')}
            <div class="w-full mt-0.5 -mb-1.5 border-t border-darkborderc/20"></div>
        {/if}
        {#if toggle.type === 'group' && toggle.children.length > 0}
            <div class="w-full mt-1">
                <Accordion styled name={toggle.value}>
                    {@render toggles((toggle as sidebarToggleGroup).children, reverse)}
                </Accordion>
            </div>
        {:else if toggle.type === 'select'}
            <div class="w-full flex gap-2 mt-2 items-center justify-between min-h-10 rounded-md px-1 transition-colors {dirtyClass(toggle.key)}">
                <span class="min-w-0 break-words">{@render getToggleDisplayName(toggle)}</span>
                <SelectInput
                    className="w-32 shrink-0"
                    value={getGlobalChatVarNH(`toggle_${toggle.key}`)}
                    onchange={(e) => {
                        setGlobalChatVar(`toggle_${toggle.key}`, e.currentTarget.value)
                    }}
                >
                    {#each toggle.options as option, i}
                        <OptionInput value={i.toString()}>{option}</OptionInput>
                    {/each}
                </SelectInput>
            </div>
        {:else if toggle.type === 'text'}
            <div class="w-full flex gap-2 mt-2 items-center justify-between min-h-10 rounded-md px-1 transition-colors {dirtyClass(toggle.key)}">
                <span class="min-w-0 break-words">{@render getToggleDisplayName(toggle)}</span>
                <TextInput
                    className="w-32 shrink-0"
                    value={getGlobalChatVarNH(`toggle_${toggle.key}`)}
                    onchange={(e) => {
                        setGlobalChatVar(`toggle_${toggle.key}`, e.currentTarget.value)
                    }}
                />
            </div>
        {:else if toggle.type === 'textarea'}
            <div class="w-full flex gap-2 mt-2 items-start justify-between min-h-10 rounded-md px-1 transition-colors {dirtyClass(toggle.key)}">
                <span class="min-w-0 break-words mt-1.5">{@render getToggleDisplayName(toggle)}</span>
                <TextAreaInput
                    className="w-32 shrink-0"
                    height="20"
                    value={getGlobalChatVarNH(`toggle_${toggle.key}`)}
                    onchange={(e) => {
                        //check is div
                        if (e.currentTarget instanceof HTMLDivElement) {
                            setGlobalChatVar(`toggle_${toggle.key}`, e.currentTarget.innerText)
                        } else {
                            setGlobalChatVar(`toggle_${toggle.key}`, e.currentTarget.value)
                        }
                    }}
                />
            </div>
        {:else if toggle.type === 'caption'}
            <div class="w-full mt-1 text-xs text-textcolor2">
                {toggle.value}
            </div>
        {:else if toggle.type === 'divider'}
            <!-- Prevent multiple dividers appearing in a row -->
            {#if index === 0 || items[index - 1]?.type !== 'divider' || items[index - 1]?.value !== toggle.value}
                <div class="w-full min-h-5 flex gap-2 mt-2 items-center" class:justify-end={!reverse}>
                    {#if toggle.value}
                        <span class="shrink-0">{@render getToggleDisplayName(toggle)}</span>
                    {/if}
                    <hr class="border-t border-darkborderc m-0 grow" />
                </div>
            {/if}
        {:else}
            <SwitchInput
                check={getGlobalChatVarNH(`toggle_${toggle.key}`) === '1'}
                name={toggle.value}
                highlight={isToggleDirty(toggle.key)}
                onChange={(checked) => {
                    setGlobalChatVar(`toggle_${toggle.key}`, checked ? '1' : '0')
                }}
            >
                {@render localToggle(toggle)}
            </SwitchInput>
        {/if}
    {/each}
{/snippet}

<div class="flex flex-col gap-4 w-full mt-3">
    {#if !DBState.db.customSidebarItems?.some((item) => item.type === 'model')}<ModelBind />{/if}
    {#if !DBState.db.customSidebarItems?.some((item) => item.type === 'persona')}<PersonaBind />{/if}
    <ToggleBind />
</div>
{#if !noContainer && groupedToggles.length > 4}
    <div class="h-48 border-darkborderc p-2 border rounded-sm flex flex-col items-start mt-2 overflow-y-auto">
        <CustomSideBar />

        {#if hasJailbreakPrompt}
            <SwitchInput bind:check={DBState.db.jailbreakToggle} name={language.jailbreakToggle} />
        {/if}

        {@render toggles(groupedToggles, true)}
        {#if chara && (DBState.db.supaModelType !== 'none' || DBState.db.hanuraiEnable || DBState.db.hypaV3)}
            <SwitchInput
                bind:check={chara.supaMemory}
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
    <CustomSideBar />

    {#if hasJailbreakPrompt}
        <SwitchInput bind:check={DBState.db.jailbreakToggle} name={language.jailbreakToggle} />
    {/if}
    {@render toggles(groupedToggles)}
    {#if chara && (DBState.db.supaModelType !== 'none' || DBState.db.hanuraiEnable || DBState.db.hypaV3)}
        <SwitchInput
            bind:check={chara.supaMemory}
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
            check={getCurrentChat()?.useLocallySetGlobalVariables}
            name={language.localToggles}
            onChange={(checked) => {
                const chatIndx = DBState.db.characters[$selectedCharID].chatPage
                const chat = DBState.db.characters[$selectedCharID].chats[chatIndx]
                if (chat) {
                    chat.useLocallySetGlobalVariables = checked
                }
            }}
        />
    {/if}
{/if}
