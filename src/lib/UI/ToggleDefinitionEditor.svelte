<script lang="ts">
    import { untrack } from 'svelte'
    import { language } from 'src/lang'
    import { RISU_TOGGLE_DRAG_TYPE } from 'src/ts/dragTypes'
    import { htmlTouchDrag } from 'src/ts/gui/htmlTouchDrag'
    import { backNavigationLayer } from 'src/ts/ui/modalNavigation'
    import { parseToggleSyntax } from 'src/ts/util'
    import {
        addToggleRow,
        groupSidebarToggles,
        missingToggleFields,
        moveToggleRow,
        parseToggleDefinitions,
        placeToggleRow,
        removeToggleRow,
        sanitizeToggleField,
        serializeToggleDefinitions,
        setToggleRowField,
        setToggleRowKind,
        toggleLineFields,
        toggleOutline,
        type ToggleFields,
        type ToggleGap,
        type ToggleLine,
        type ToggleLineKind,
    } from 'src/ts/toggleDefinitions'
    import ArrowDown from '@lucide/svelte/icons/arrow-down'
    import ArrowUp from '@lucide/svelte/icons/arrow-up'
    import GripVertical from '@lucide/svelte/icons/grip-vertical'
    import Maximize2 from '@lucide/svelte/icons/maximize-2'
    import Plus from '@lucide/svelte/icons/plus'
    import Trash from '@lucide/svelte/icons/trash'
    import Help from '../Others/Help.svelte'
    import ToggleItems from '../SideBars/ToggleItems.svelte'
    import OptionInput from './GUI/OptionInput.svelte'
    import SelectInput from './GUI/SelectInput.svelte'
    import TextAreaInput from './GUI/TextAreaInput.svelte'
    import TextInput from './GUI/TextInput.svelte'
    import ToggleDefinitionEditor from './ToggleDefinitionEditor.svelte'
    import ToggleEditorWindow, { toggleEditorWindow } from './ToggleEditorWindow.svelte'

    interface Props {
        /** The stored toggle definition text. Anything other than a string shows as empty. */
        value: unknown
        /** Receives the whole text after each edit the user makes. */
        onChange: (value: string) => void
        className?: string
        /** Leaves out the editor's own label and help, for a host that already names the editor. */
        hideLabel?: boolean
    }

    let { value, onChange, className = '', hideLabel = false }: Props = $props()

    type RowKind = Exclude<ToggleLineKind, 'groupEnd'>
    type View = 'edit' | 'preview' | 'raw'
    const rowKinds: RowKind[] = ['switch', 'select', 'text', 'textarea', 'group', 'divider', 'caption']
    const keyed = (kind: ToggleLineKind | null) => kind === 'switch' || kind === 'select' || kind === 'text' || kind === 'textarea'
    const actionButton = 'size-10 flex items-center justify-center rounded-md text-textcolor2 transition-colors disabled:opacity-40'
    // Row cells: stacked under 448 px of rows width, two lines from 448 px, one line from 672 px.
    const keyCell = 'col-start-2 col-span-2 row-start-2 @md/rows:col-span-1 @md/rows:col-start-2 @2xl/rows:col-start-3 @2xl/rows:row-start-1'
    const nameKeyedCell = 'col-start-2 col-span-2 row-start-3 @md/rows:col-start-3 @md/rows:col-span-1 @md/rows:row-start-1 @2xl/rows:col-start-4'
    const nameOnlyCell = 'col-start-2 col-span-2 row-start-2 @md/rows:col-start-3 @md/rows:col-span-1 @md/rows:row-start-1 @2xl/rows:col-span-2 @2xl/rows:col-start-3'
    const optionsCell = 'col-start-2 col-span-2 row-start-4 @md/rows:col-start-3 @md/rows:row-start-2 @2xl/rows:col-span-3 @2xl/rows:col-start-3'

    // The pop-out window this editor is drawn in, if any.
    const popout = toggleEditorWindow()

    const text = $derived(typeof value === 'string' ? value : '')
    // svelte-ignore state_referenced_locally
    let lines = $state.raw<ToggleLine[]>(parseToggleDefinitions(text))
    // The text `lines` stands for. An incoming value that differs came from elsewhere and is read again.
    // svelte-ignore state_referenced_locally
    let shown = text
    // svelte-ignore state_referenced_locally
    let rawDraft = $state(text)
    let view = $state<View>('edit')
    let windowOpen = $state(false)
    // Values the preview switches hold. They stay in this editor and are never saved.
    let previewValues = $state<Record<string, string>>({})
    let addMenu = $state<{ group: number | null; up: boolean } | null>(null)
    // The line id of the row or group being dragged, and the gap it would be placed at.
    let draggedId = $state<number | null>(null)
    let dropGap = $state<ToggleGap | null>(null)
    let rowsElement: HTMLDivElement | undefined = $state()

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
    // The preview list leaves out empty groups.
    const previewShown = $derived(preview.some((toggle) => toggle.type !== 'group' || toggle.children.length > 0))
    // A wide pop-out window keeps the preview beside the rows instead of in its own view.
    const sideBySide = $derived(popout?.wide ?? false)
    const views = $derived<View[]>(sideBySide ? ['edit', 'raw'] : ['edit', 'preview', 'raw'])
    const shownView = $derived(sideBySide && view === 'preview' ? 'edit' : view)

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

    function missingMessage(missing: Array<keyof ToggleFields>) {
        const copy = language.risuNest.toggleEditor
        if (missing.length > 1) return copy.missingKeyAndName
        return missing[0] === 'key' ? copy.missingKey : copy.missingName
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

    function viewName(option: View) {
        if (option === 'edit') return language.hotkeyDesc.edit
        return option === 'preview' ? language.preview : language.risuNest.toggleEditor.raw
    }

    function toggleAddMenu(event: MouseEvent & { currentTarget: HTMLButtonElement }, group: number | null) {
        if (addMenu?.group === group) {
            addMenu = null
            return
        }
        // Open toward the side with more room so the whole menu can be scrolled into view.
        const box = event.currentTarget.getBoundingClientRect()
        addMenu = { group, up: box.top > window.innerHeight - box.bottom }
    }

    function addRow(kind: RowKind, group: number | null) {
        addMenu = null
        commit(addToggleRow(lines, kind, group))
    }

    $effect(() => {
        if (!addMenu) return
        const owner = String(addMenu.group ?? 'top')
        const dismiss = (event: PointerEvent) => {
            const control = event.target instanceof Element ? event.target.closest('[data-toggle-add]') : null
            if (control?.getAttribute('data-toggle-add') !== owner) addMenu = null
        }
        document.addEventListener('pointerdown', dismiss)
        return () => {
            document.removeEventListener('pointerdown', dismiss)
        }
    })

    const isToggleDrag = (event: DragEvent) => event.dataTransfer?.types.includes(RISU_TOGGLE_DRAG_TYPE) ?? false
    // Only a drag that started in this editor is placed here.
    const ownDrag = (event: DragEvent) => draggedId !== null && isToggleDrag(event)

    function startDrag(event: DragEvent & { currentTarget: HTMLElement }, id: number) {
        if (!event.dataTransfer) return
        draggedId = id
        dropGap = null
        event.dataTransfer.effectAllowed = 'move'
        event.dataTransfer.setData(RISU_TOGGLE_DRAG_TYPE, String(id))
        const row = event.currentTarget.closest<HTMLElement>('[data-toggle-row]')
        if (row) {
            const box = row.getBoundingClientRect()
            event.dataTransfer.setDragImage(row, event.clientX - box.left, event.clientY - box.top)
        }
    }

    function endDrag() {
        draggedId = null
        dropGap = null
    }

    /** The gap a dragged item would be placed at for a pointer at this height. A group only goes between top-level items. */
    function gapAt(y: number): ToggleGap | null {
        if (draggedId === null || !rowsElement) return null
        const movingGroup = lines.find((line) => line.id === draggedId)?.kind === 'group'
        const items = (parent: Element) => [...parent.children]
            .filter((child): child is HTMLElement => child instanceof HTMLElement && child.dataset.toggleItem !== undefined)
        const idOf = (item: HTMLElement) => Number(item.dataset.toggleItem)
        const middle = (element: Element) => {
            const box = element.getBoundingClientRect()
            return box.top + box.height / 2
        }
        for (const item of items(rowsElement)) {
            const children = [...item.children].find((child) => child instanceof HTMLElement && child.dataset.toggleChildren !== undefined)
            if (children && !movingGroup) {
                const header = item.querySelector('[data-toggle-row]')
                if (header && y < middle(header)) return { group: null, before: idOf(item) }
                if (y < item.getBoundingClientRect().bottom) {
                    const next = items(children).find((child) => y < middle(child))
                    return { group: idOf(item), before: next ? idOf(next) : null }
                }
            } else if (y < middle(item)) {
                return { group: null, before: idOf(item) }
            }
        }
        return { group: null, before: null }
    }

    function dragEnter(event: DragEvent) {
        if (!ownDrag(event)) return
        event.preventDefault()
        event.stopPropagation()
    }

    function dragOver(event: DragEvent) {
        if (!ownDrag(event)) return
        event.preventDefault()
        event.stopPropagation()
        if (event.dataTransfer) event.dataTransfer.dropEffect = 'move'
        dropGap = gapAt(event.clientY)
    }

    function dragLeave(event: DragEvent) {
        if (!ownDrag(event)) return
        if (!(event.relatedTarget instanceof Node) || !rowsElement?.contains(event.relatedTarget)) dropGap = null
    }

    function drop(event: DragEvent) {
        if (!ownDrag(event)) return
        event.preventDefault()
        event.stopPropagation()
        const id = draggedId!
        const gap = gapAt(event.clientY)
        endDrag()
        if (gap) commit(placeToggleRow(lines, id, gap))
    }

    const gapShown = (group: number | null, before: number | null) => dropGap?.group === group && dropGap.before === before
</script>

{#snippet dropLine(place: string)}
    <div class="absolute h-0.5 rounded-full bg-borderc pointer-events-none {place}" aria-hidden="true"></div>
{/snippet}

{#snippet addControl(group: number | null)}
    {@const open = addMenu !== null && addMenu.group === group}
    <div class="relative self-start" class:mt-1={group === null} data-toggle-add={group ?? 'top'}>
        {#if open}
            <div class="absolute left-0 z-20 w-48 rounded-md border border-darkborderc bg-darkbg shadow-lg p-1 flex flex-col max-h-72 overflow-y-auto {addMenu?.up ? 'bottom-full mb-1' : 'top-full mt-1'}" role="menu" aria-label={language.add} use:backNavigationLayer={{ close: () => { addMenu = null } }}>
                {#each rowKinds as kind (kind)}
                    {#if group === null || kind !== 'group'}
                        <button role="menuitem" class="px-3 py-2 text-left text-sm rounded-sm hover:bg-selected" onclick={() => addRow(kind, group)}>{kindName(kind)}</button>
                    {/if}
                {/each}
            </div>
        {/if}
        {#if group === null}
            <button class="bg-transparent hover:bg-darkbg focus:ring-selected border-darkborderc text-textcolor2 border rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 px-3 py-1.5 text-sm inline-flex items-center gap-1.5" aria-haspopup="menu" aria-expanded={open} onclick={(event) => toggleAddMenu(event, group)}><Plus size={16} />{language.add}</button>
        {:else}
            <button class="inline-flex items-center gap-1 px-2 py-1 text-sm text-textcolor2 hover:text-textcolor rounded-md transition-colors" aria-haspopup="menu" aria-expanded={open} onclick={(event) => toggleAddMenu(event, group)}><Plus size={14} />{language.add}</button>
        {/if}
    </div>
{/snippet}

{#snippet row(line: ToggleLine, nested: boolean, first: boolean, last: boolean)}
    {@const fields = toggleLineFields(line.raw)}
    {@const missing = missingToggleFields(line.raw)}
    {@const missingId = `toggle-row-${line.id}-missing`}
    <div class="grid items-center gap-x-2 gap-y-1.5 rounded-md border border-darkborderc bg-darkbg px-1 py-1 grid-cols-[1.5rem_minmax(0,1fr)_auto] @md/rows:grid-cols-[1.5rem_9rem_minmax(0,1fr)_auto] @2xl/rows:grid-cols-[1.5rem_9rem_11rem_minmax(0,1fr)_auto]" data-toggle-row>
        <span class="col-start-1 row-start-1 h-10 flex items-center justify-center text-textcolor2 cursor-grab" draggable="true" aria-hidden="true" ondragstart={(event) => startDrag(event, line.id)} ondragend={endDrag}><GripVertical size={16} /></span>
        <SelectInput size="sm" className="min-w-0 col-start-2 row-start-1" ariaLabel={language.type} value={line.kind ?? 'switch'} onchange={(event) => {
            commit(setToggleRowKind(lines, line.id, event.currentTarget.value as RowKind))
        }}>
            {#each rowKinds as kind (kind)}
                {#if !nested || kind !== 'group'}
                    <OptionInput value={kind}>{kindName(kind)}</OptionInput>
                {/if}
            {/each}
        </SelectInput>
        <div class="flex items-center col-start-3 row-start-1 @md/rows:col-start-4 @2xl/rows:col-start-5">
            <button class="{actionButton} hover:text-textcolor hover:bg-selected" title={language.movePresetUp} aria-label={language.movePresetUp} disabled={first} onclick={() => commit(moveToggleRow(lines, line.id, -1))}><ArrowUp size={16} /></button>
            <button class="{actionButton} hover:text-textcolor hover:bg-selected" title={language.movePresetDown} aria-label={language.movePresetDown} disabled={last} onclick={() => commit(moveToggleRow(lines, line.id, 1))}><ArrowDown size={16} /></button>
            <span class="mx-0.5 @md/rows:mx-1 h-5 w-px bg-darkborderc" aria-hidden="true"></span>
            <button class="{actionButton} hover:text-draculared hover:bg-draculared/15" title={language.remove} aria-label={language.remove} onclick={() => commit(removeToggleRow(lines, line.id))}><Trash size={16} /></button>
        </div>
        {#if keyed(line.kind)}
            <label class="flex items-center min-w-0 rounded-md border {missing.includes('key') ? 'border-draculared' : 'border-darkborderc'} focus-within:border-borderc focus-within:ring-2 focus-within:ring-borderc text-sm {keyCell}">
                <span class="pl-2 text-subtext">toggle_</span>
                <input class="min-w-0 flex-1 bg-transparent py-1 pr-2 text-textcolor focus:outline-hidden" aria-label={language.key} aria-invalid={missing.includes('key') || undefined} aria-describedby={missing.includes('key') ? missingId : undefined} value={fields.key} oninput={(event) => editField(line.id, 'key', event.currentTarget)} />
            </label>
        {/if}
        <TextInput size="sm" className="min-w-0 {keyed(line.kind) ? nameKeyedCell : nameOnlyCell} aria-[invalid=true]:border-draculared" ariaLabel={language.name} ariaInvalid={missing.includes('name') || undefined} ariaDescribedby={missing.includes('name') ? missingId : undefined} value={fields.name} oninput={(event) => editField(line.id, 'name', event.currentTarget)} />
        {#if line.kind === 'select'}
            <TextInput size="sm" className="min-w-0 {optionsCell}" ariaLabel={language.options} value={fields.options} oninput={(event) => editField(line.id, 'options', event.currentTarget)} />
        {/if}
        {#if missing.length > 0}
            <p id={missingId} class="text-xs text-draculared col-start-2 col-span-full">{missingMessage(missing)}</p>
        {/if}
    </div>
{/snippet}

{#snippet rowList()}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
        bind:this={rowsElement}
        class="@container/rows flex flex-col gap-1.5 min-w-0"
        use:htmlTouchDrag={{ item: '[data-toggle-row]' }}
        ondragenter={dragEnter}
        ondragover={dragOver}
        ondragleave={dragLeave}
        ondrop={drop}
    >
        {#each outline as item, index (lines[item.line].id)}
            {@const id = lines[item.line].id}
            {@const lastItem = index === outline.length - 1}
            <div class="relative flex flex-col gap-1.5 transition-transform {draggedId === id ? 'opacity-50 scale-[0.98]' : ''}" data-toggle-item={id}>
                {#if gapShown(null, id)}{@render dropLine('inset-x-0 -top-1')}{/if}
                {@render row(lines[item.line], false, index === 0, lastItem)}
                {#if item.type === 'group'}
                    <div class="relative ml-1 pl-1.5 @md/rows:ml-3 @md/rows:pl-3 border-l-2 border-selected flex flex-col gap-1.5" data-toggle-children>
                        {#each item.children as child, childIndex (lines[child].id)}
                            {@const childId = lines[child].id}
                            <div class="relative transition-transform {draggedId === childId ? 'opacity-50 scale-[0.98]' : ''}" data-toggle-item={childId}>
                                {#if gapShown(id, childId)}{@render dropLine('inset-x-0 -top-1')}{/if}
                                {@render row(lines[child], true, false, false)}
                                {#if childIndex === item.children.length - 1 && gapShown(id, null)}{@render dropLine('inset-x-0 -bottom-1')}{/if}
                            </div>
                        {/each}
                        {#if item.children.length === 0 && gapShown(id, null)}{@render dropLine('left-1.5 @md/rows:left-3 right-0 -top-1')}{/if}
                        {@render addControl(id)}
                    </div>
                {/if}
                {#if lastItem && gapShown(null, null)}{@render dropLine('inset-x-0 -bottom-1')}{/if}
            </div>
        {/each}
        {@render addControl(null)}
    </div>
{/snippet}

{#snippet previewBox(className: string)}
    <div class="rounded-md border border-darkborderc bg-darkbg p-3 self-start min-w-0 w-full {className}">
        <div class="flex flex-col">
            <ToggleItems
                items={preview}
                value={(name) => previewValues[name] ?? ''}
                onEdit={(name, next) => { previewValues[name] = next }}
                openGroups
            />
        </div>
    </div>
{/snippet}

<div class="flex flex-col gap-2 {className}">
    <div class="flex flex-wrap items-center gap-2">
        {#if !hideLabel}
            <div class="flex items-center gap-2">
                <span class="text-textcolor">{language.customPromptTemplateToggle}</span>
                <Help key="customPromptTemplateToggle" />
            </div>
        {/if}
        <div class="flex items-center gap-1">
            <div class="flex rounded-md border border-darkborderc text-sm" role="group" aria-label={language.risuNest.toggleEditor.view}>
                {#each views as option, index (option)}
                    <button class="px-2.5 py-1 {index === 0 ? 'rounded-l-md' : 'border-l border-darkborderc'} {index === views.length - 1 ? 'rounded-r-md' : ''} {option === shownView ? 'bg-darkbutton' : 'text-textcolor2 hover:text-textcolor'}" aria-pressed={option === shownView} onclick={() => { view = option }}>{viewName(option)}</button>
                {/each}
            </div>
            {#if !popout}
                <button class="size-8 flex items-center justify-center rounded-md text-textcolor2 hover:text-textcolor hover:bg-selected transition-colors" title={language.risuNest.textEditor.open} aria-label={language.risuNest.textEditor.open} onclick={() => { windowOpen = true }}><Maximize2 size={16} /></button>
            {/if}
        </div>
    </div>
    {#if shownView === 'raw'}
        <TextAreaInput bind:value={rawDraft} optimaizedInput={false} onInput={commitRaw} onchange={commitRaw} />
    {:else if shownView === 'preview'}
        {#if previewShown}{@render previewBox('max-w-sm')}{/if}
    {:else if sideBySide}
        <div class="grid gap-4 grid-cols-[minmax(0,1fr)_20rem]">
            {@render rowList()}
            {#if previewShown}
                <div class="sticky top-0 self-start">
                    <div class="text-xs text-subtext mb-1">{language.preview}</div>
                    {@render previewBox('')}
                </div>
            {/if}
        </div>
    {:else}
        {@render rowList()}
    {/if}
</div>
{#if windowOpen}
    <ToggleEditorWindow title={language.customPromptTemplateToggle} onClose={() => { windowOpen = false }}>
        <ToggleDefinitionEditor value={text} {onChange} hideLabel />
    </ToggleEditorWindow>
{/if}
