<script lang="ts">
    import { untrack } from 'svelte'
    import { language } from 'src/lang'
    import { parseToggleSyntax } from 'src/ts/util'
    import {
        addToggleRow,
        groupSidebarToggles,
        moveToggleRow,
        parseToggleDefinitions,
        removeToggleRow,
        sanitizeToggleField,
        serializeToggleDefinitions,
        setToggleRowField,
        setToggleRowKind,
        toggleLineFields,
        toggleOutline,
        type ToggleFields,
        type ToggleLine,
        type ToggleLineKind,
    } from 'src/ts/toggleDefinitions'
    import ArrowDown from '@lucide/svelte/icons/arrow-down'
    import ArrowUp from '@lucide/svelte/icons/arrow-up'
    import Plus from '@lucide/svelte/icons/plus'
    import Trash from '@lucide/svelte/icons/trash'
    import Help from '../Others/Help.svelte'
    import ToggleItems from '../SideBars/ToggleItems.svelte'
    import OptionInput from './GUI/OptionInput.svelte'
    import SelectInput from './GUI/SelectInput.svelte'
    import TextAreaInput from './GUI/TextAreaInput.svelte'
    import TextInput from './GUI/TextInput.svelte'

    interface Props {
        /** The stored toggle definition text. Anything other than a string shows as empty. */
        value: unknown
        /** Receives the whole text after each edit the user makes. */
        onChange: (value: string) => void
        className?: string
    }

    let { value, onChange, className = '' }: Props = $props()

    type RowKind = Exclude<ToggleLineKind, 'groupEnd'>
    const rowKinds: RowKind[] = ['switch', 'select', 'text', 'textarea', 'group', 'divider', 'caption']
    const keyed = (kind: ToggleLineKind | null) => kind === 'switch' || kind === 'select' || kind === 'text' || kind === 'textarea'
    const actionButton = 'size-10 flex items-center justify-center rounded-md text-textcolor2 transition-colors disabled:opacity-40'

    const text = $derived(typeof value === 'string' ? value : '')
    // svelte-ignore state_referenced_locally
    let lines = $state.raw<ToggleLine[]>(parseToggleDefinitions(text))
    // The text `lines` stands for. An incoming value that differs came from elsewhere and is read again.
    // svelte-ignore state_referenced_locally
    let shown = text
    // svelte-ignore state_referenced_locally
    let rawDraft = $state(text)
    let rawView = $state(false)
    // Values the preview switches hold. They stay in this editor and are never saved.
    let previewValues = $state<Record<string, string>>({})

    $effect.pre(() => {
        const incoming = text
        untrack(() => {
            if (incoming === shown) return
            shown = incoming
            rawDraft = incoming
            lines = parseToggleDefinitions(incoming)
        })
    })

    const outline = $derived(toggleOutline(lines))
    const preview = $derived(groupSidebarToggles(parseToggleSyntax(serializeToggleDefinitions(lines))))

    function commit(next: ToggleLine[]) {
        lines = next
        const saved = serializeToggleDefinitions(next)
        if (saved === shown) return
        shown = saved
        rawDraft = saved
        onChange(saved)
    }

    function commitRaw() {
        if (rawDraft === shown) return
        shown = rawDraft
        lines = parseToggleDefinitions(rawDraft)
        onChange(rawDraft)
    }

    function editField(id: number, field: keyof ToggleFields, input: HTMLInputElement) {
        const next = sanitizeToggleField(input.value)
        if (next !== input.value) input.value = next
        commit(setToggleRowField(lines, id, field, next))
    }

    function kindName(kind: RowKind) {
        const copy = language.risuNest.toggleEditor
        switch (kind) {
            case 'switch': return copy.switch
            case 'select': return copy.select
            case 'text': return copy.text
            case 'textarea': return copy.textarea
            case 'group': return copy.group
            case 'divider': return copy.divider
            case 'caption': return copy.caption
        }
    }
</script>

{#snippet row(line: ToggleLine, nested: boolean, first: boolean, last: boolean)}
    {@const fields = toggleLineFields(line.raw)}
    <div class="grid items-center gap-x-2 gap-y-1.5 rounded-md border border-darkborderc bg-darkbg px-2 py-1.5 grid-cols-[minmax(0,1fr)_auto] @md/rows:grid-cols-[9rem_minmax(0,1fr)_auto]">
        <SelectInput size="sm" className="min-w-0" ariaLabel={language.type} value={line.kind ?? 'switch'} onchange={(event) => {
            commit(setToggleRowKind(lines, line.id, event.currentTarget.value as RowKind))
        }}>
            {#each rowKinds as kind (kind)}
                {#if !nested || kind !== 'group'}
                    <OptionInput value={kind}>{kindName(kind)}</OptionInput>
                {/if}
            {/each}
        </SelectInput>
        <div class="flex items-center col-start-2 row-start-1 @md/rows:col-start-3">
            <button class="{actionButton} hover:text-textcolor hover:bg-selected" title={language.movePresetUp} aria-label={language.movePresetUp} disabled={first} onclick={() => commit(moveToggleRow(lines, line.id, -1))}><ArrowUp size={16} /></button>
            <button class="{actionButton} hover:text-textcolor hover:bg-selected" title={language.movePresetDown} aria-label={language.movePresetDown} disabled={last} onclick={() => commit(moveToggleRow(lines, line.id, 1))}><ArrowDown size={16} /></button>
            <button class="{actionButton} hover:text-draculared hover:bg-draculared/15" title={language.remove} aria-label={language.remove} onclick={() => commit(removeToggleRow(lines, line.id))}><Trash size={16} /></button>
        </div>
        {#if keyed(line.kind)}
            <label class="flex items-center min-w-0 rounded-md border border-darkborderc focus-within:border-borderc focus-within:ring-2 focus-within:ring-borderc text-sm col-span-2 col-start-1 @md/rows:col-span-1 @md/rows:col-start-1 @md/rows:row-start-2">
                <span class="pl-2 text-subtext">toggle_</span>
                <input class="min-w-0 flex-1 bg-transparent py-1 pr-2 text-textcolor focus:outline-hidden" aria-label={language.key} value={fields.key} oninput={(event) => editField(line.id, 'key', event.currentTarget)} />
            </label>
        {/if}
        <TextInput size="sm" className="min-w-0 col-span-2 col-start-1 @md/rows:col-span-1 @md/rows:col-start-2 @md/rows:row-start-1" ariaLabel={language.name} value={fields.name} oninput={(event) => editField(line.id, 'name', event.currentTarget)} />
        {#if line.kind === 'select'}
            <TextInput size="sm" className="min-w-0 col-span-2 col-start-1 @md/rows:col-span-2 @md/rows:col-start-2 @md/rows:row-start-2" ariaLabel={language.options} value={fields.options} oninput={(event) => editField(line.id, 'options', event.currentTarget)} />
        {/if}
    </div>
{/snippet}

<div class="@container/toggles flex flex-col gap-2 {className}">
    <div class="flex items-center gap-2">
        <span class="text-textcolor">{language.customPromptTemplateToggle}</span>
        <Help key="customPromptTemplateToggle" />
        <div class="ml-auto flex rounded-md border border-darkborderc text-sm" role="group" aria-label={language.risuNest.toggleEditor.view}>
            <button class="px-3 py-1 rounded-l-md {rawView ? 'text-textcolor2 hover:text-textcolor' : 'bg-darkbutton'}" aria-pressed={!rawView} onclick={() => { rawView = false }}>{language.hotkeyDesc.edit}</button>
            <button class="px-3 py-1 rounded-r-md border-l border-darkborderc {rawView ? 'bg-darkbutton' : 'text-textcolor2 hover:text-textcolor'}" aria-pressed={rawView} onclick={() => { rawView = true }}>{language.risuNest.toggleEditor.raw}</button>
        </div>
    </div>
    {#if rawView}
        <TextAreaInput bind:value={rawDraft} optimaizedInput={false} onInput={commitRaw} onchange={commitRaw} />
    {:else}
        <div class="grid gap-3 grid-cols-1 @2xl/toggles:grid-cols-[minmax(0,1fr)_15rem]">
            <div class="@container/rows flex flex-col gap-1.5 min-w-0">
                {#each outline as item, index (lines[item.line].id)}
                    {#if item.type === 'group'}
                        <div class="flex flex-col gap-1.5">
                            {@render row(lines[item.line], false, index === 0, index === outline.length - 1)}
                            {#if item.children.length > 0}
                                <div class="ml-3 pl-3 border-l-2 border-selected flex flex-col gap-1.5">
                                    {#each item.children as child (lines[child].id)}
                                        {@render row(lines[child], true, false, false)}
                                    {/each}
                                </div>
                            {/if}
                        </div>
                    {:else}
                        {@render row(lines[item.line], false, index === 0, index === outline.length - 1)}
                    {/if}
                {/each}
                <button class="bg-transparent hover:bg-darkbg focus:ring-selected border-darkborderc text-textcolor2 border rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 px-3 py-1.5 text-sm inline-flex items-center gap-1.5 self-start mt-1" onclick={() => commit(addToggleRow(lines))}><Plus size={16} />{language.add}</button>
            </div>
            {#if preview.length > 0}
                <div class="rounded-md border border-darkborderc bg-darkbg p-3 self-start min-w-0 w-full">
                    <div class="text-xs text-subtext">{language.preview}</div>
                    <div class="flex flex-col mt-1">
                        <ToggleItems
                            items={preview}
                            value={(name) => previewValues[name] ?? ''}
                            onEdit={(name, next) => { previewValues[name] = next }}
                        />
                    </div>
                </div>
            {/if}
        </div>
    {/if}
</div>
