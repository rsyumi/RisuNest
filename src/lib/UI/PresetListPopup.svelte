<script module lang="ts">
    export type PresetGuard = (work: () => Promise<void>) => Promise<void>

    /**
     * Replaces the popup's own edits, which write the stored list directly. Each action resolves
     * to whether it changed the list; the open menu follows only a change.
     */
    export interface PresetListActions {
        rename(index: number): Promise<boolean>
        duplicate(index: number): Promise<boolean>
        remove(index: number): Promise<boolean>
        /** Moves the preset at `from` into the gap before `to`, from 0 to the list length. */
        move(from: number, to: number): Promise<boolean>
    }
</script>

<script lang="ts" generics="T extends { name: string }">
    import { untrack, type Snippet } from 'svelte'
    import CheckIcon from '@lucide/svelte/icons/check'
    import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
    import ChevronUpIcon from '@lucide/svelte/icons/chevron-up'
    import CopyIcon from '@lucide/svelte/icons/copy'
    import EllipsisVerticalIcon from '@lucide/svelte/icons/ellipsis-vertical'
    import GripVerticalIcon from '@lucide/svelte/icons/grip-vertical'
    import PencilIcon from '@lucide/svelte/icons/pencil'
    import TrashIcon from '@lucide/svelte/icons/trash'
    import XIcon from '@lucide/svelte/icons/x'
    import { language } from 'src/lang'
    import { alertInput, alertToast } from 'src/ts/alert'
    import { htmlTouchDrag, type HtmlTouchDragOptions } from 'src/ts/gui/htmlTouchDrag'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'

    interface Props {
        title: string
        close: () => void
        /** The stored list, created on first use. */
        list: () => T[]
        /** The stored list as last read, absent before it exists. */
        presets: T[] | undefined
        empty?: string
        /** Shown when presets exist but the filter hides all of them. */
        emptyFiltered?: string
        filter?: (preset: T) => boolean
        movable?: boolean
        rowTitle?: string
        subtitle?: (preset: T) => string
        selected?: (preset: T, index: number) => boolean
        apply: (preset: T, index: number) => Promise<void>
        copy?: (preset: T, name: string) => T
        confirmDelete?: (preset: T) => Promise<boolean>
        actions?: PresetListActions
        /** Drag type of a row. With it, rows show a handle and can be dragged while movable. */
        dragType?: string
        /** Position and stacking of the backdrop. */
        layer?: string
        width?: string
        /** Scrolls only the rows, keeping the header and footer in view. */
        listScroll?: boolean
        /** Enlarges rows and their menu button on touch screens. */
        touchTargets?: boolean
        header?: Snippet<[PresetGuard]>
        leading?: Snippet<[PresetGuard]>
        /** Content before the name. With it, the selected mark follows the name. */
        rowLeading?: Snippet<[T, number]>
        rowTrailing?: Snippet<[T, number]>
        rowActions?: Snippet<[T, number, PresetGuard, string]>
        /** Actions between duplicate and delete. */
        rowActionsEnd?: Snippet<[T, number, PresetGuard, string]>
        footer?: Snippet<[PresetGuard, string]>
    }
    let {
        title,
        close,
        list,
        presets,
        empty,
        emptyFiltered,
        filter,
        movable = true,
        rowTitle,
        subtitle,
        selected,
        apply,
        copy,
        confirmDelete,
        actions,
        dragType,
        layer = 'fixed inset-0 z-modal',
        width = 'w-96',
        listScroll = false,
        touchTargets = false,
        header,
        leading,
        rowLeading,
        rowTrailing,
        rowActions,
        rowActionsEnd,
        footer,
    }: Props = $props()

    const titleId = $props.id()
    let openMenu = $state<number | null>(null)
    /** Nested alerts in progress. Escape and Back belong to them, not to this popup. */
    let busy = $state(0)
    let visible = $derived(
        (presets ?? [])
            .map((preset, index) => ({ preset, index }))
            .filter(({ preset }) => !filter || filter(preset)),
    )
    let sortable = $derived(!!dragType && movable)
    let dialog = $state<HTMLElement>()
    /** The dialog's height when filtering began, kept so a search does not move the dialog under the field. */
    let filteringHeight = $state<number | null>(null)
    $effect.pre(() => {
        const filtering = listScroll && visible.length < (presets?.length ?? 0)
        untrack(() => {
            if (!filtering) filteringHeight = null
            else if (filteringHeight === null && dialog) filteringHeight = dialog.offsetHeight
        })
    })
    let dragged: T | null = null
    let dragFrom = $state<number | null>(null)
    /** The gap a drop would move the dragged row into, from 0 to the list length. */
    let dropAt = $state<number | null>(null)

    /** Touch dragging, for lists whose rows can be dragged. */
    const touchDrag = (node: HTMLElement, options: HtmlTouchDragOptions | null) =>
        options ? htmlTouchDrag(node, options) : undefined

    const guarded: PresetGuard = async (work) => {
        busy++
        try {
            await work()
        } finally {
            busy--
        }
    }
    const dismiss = () => {
        if (!busy) close()
    }

    const rename = (index: number) =>
        guarded(async () => {
            if (actions) {
                await actions.rename(index)
                return
            }
            const preset = list()[index]
            if (!preset) return
            const name = (await alertInput(language.renamePreset, [], preset.name))?.trim()
            if (!name || name === preset.name) return
            if (!list().includes(preset)) return
            const previous = preset.name
            preset.name = name
            alertToast(language.presetRenamed(previous, name))
        })
    function duplicate(index: number) {
        if (actions) {
            void guarded(async () => {
                if (await actions.duplicate(index)) openMenu = null
            })
            return
        }
        const current = list()
        const preset = current[index]
        if (!preset || !copy) return
        const duplicated = copy(preset, `${preset.name} (Copy)`)
        current.splice(index + 1, 0, duplicated)
        openMenu = null
        alertToast(language.presetDuplicated(duplicated.name))
    }
    const remove = (index: number) =>
        guarded(async () => {
            if (actions) {
                if (await actions.remove(index)) openMenu = null
                return
            }
            const preset = list()[index]
            if (!preset || !confirmDelete || !(await confirmDelete(preset))) return
            const current = list()
            const currentIndex = current.indexOf(preset)
            if (currentIndex < 0) return
            current.splice(currentIndex, 1)
            openMenu = null
            alertToast(language.presetDeleted(preset.name))
        })
    function move(index: number, delta: -1 | 1) {
        const current = list()
        const target = index + delta
        if (target < 0 || target >= current.length) return
        if (actions) {
            void guarded(async () => {
                if (await actions.move(index, delta < 0 ? target : target + 1)) openMenu = target
            })
            return
        }
        ;[current[index], current[target]] = [current[target], current[index]]
        openMenu = target
    }

    function endDrag() {
        dragged = null
        dragFrom = null
        dropAt = null
    }
    function accepts(event: DragEvent) {
        return !!dragType && dragged !== null && (event.dataTransfer?.types.includes(dragType) ?? false)
    }
    function dragStart(event: DragEvent, preset: T, index: number) {
        const data = event.dataTransfer
        const source = event.target instanceof Element ? event.target.closest('[draggable="true"]') : null
        if (!sortable || !dragType || !data || !source) return
        dragged = preset
        dragFrom = index
        data.effectAllowed = 'move'
        data.setData(dragType, String(index))
        const row = source.closest<HTMLElement>('[data-preset-row]')
        if (row) {
            const bounds = row.getBoundingClientRect()
            data.setDragImage(row, event.clientX - bounds.left, event.clientY - bounds.top)
        }
    }
    function dragOver(event: DragEvent, index: number) {
        if (!accepts(event)) return
        event.preventDefault()
        event.stopPropagation()
        event.dataTransfer!.dropEffect = 'move'
        const bounds = (event.currentTarget as HTMLElement).getBoundingClientRect()
        dropAt = event.clientY < bounds.top + bounds.height / 2 ? index : index + 1
    }
    async function drop(event: DragEvent) {
        if (!accepts(event)) return
        event.preventDefault()
        event.stopPropagation()
        const preset = dragged
        const to = dropAt
        endDrag()
        const current = list()
        const from = preset ? current.indexOf(preset) : -1
        if (from < 0 || to === null || to === from || to === from + 1) return
        if (actions) {
            await guarded(async () => {
                await actions.move(from, to)
            })
            return
        }
        const [moved] = current.splice(from, 1)
        current.splice(to > from ? to - 1 : to, 0, moved)
    }
    /** Whether the gap would move the dragged row, so a gap next to it shows nothing. */
    const movesTo = (gap: number) => dragFrom !== null && gap !== dragFrom && gap !== dragFrom + 1

    const action =
        'inline-flex items-center justify-center gap-1.5 min-h-10 px-3 rounded-md border border-darkborderc bg-darkbutton text-sm text-textcolor transition-colors hover:bg-selected disabled:opacity-40 disabled:pointer-events-none'
    const small =
        'inline-flex items-center gap-1 h-8 px-2 rounded-md border border-darkborderc bg-darkbutton text-xs text-textcolor transition-colors hover:bg-selected disabled:opacity-40 disabled:pointer-events-none'
    const dropLine = 'absolute inset-x-0 h-0.5 rounded-full bg-borderc pointer-events-none'
</script>

<div class="{layer} bg-black/50 flex justify-center items-center p-4" use:modalNavigation={{ close: dismiss }}>
    <div
        bind:this={dialog}
        class="relative z-10 bg-darkbg text-textcolor rounded-md flex flex-col {width} max-w-full max-h-full"
        style:min-height={filteringHeight === null ? null : `min(${filteringHeight}px, 100%)`}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
    >
        <div class="flex items-center gap-2 p-4 pb-2">
            <h2 id={titleId} class="font-bold grow m-0">{title}</h2>
            <button class="text-textcolor2 hover:text-green-500 transition-colors" title={language.cancel} onclick={close}>
                <XIcon size={24} />
            </button>
        </div>
        <div class="flex flex-col gap-3 px-4 pb-4 {listScroll ? 'min-h-0 flex-1' : 'overflow-y-auto'}">
            {@render header?.(guarded)}
            {#if leading}
                <div class="flex flex-col gap-1">
                    {@render leading(guarded)}
                </div>
            {/if}
            {#if !presets?.length}
                {#if empty}<p class="text-textcolor2 text-sm m-0">{empty}</p>{/if}
            {:else if visible.length === 0}
                <p class="text-textcolor2 text-sm m-0 {listScroll ? 'flex-1' : ''}">{emptyFiltered ?? empty}</p>
            {:else}
                <div
                    class="flex flex-col gap-1 {listScroll ? 'overflow-y-auto min-h-0 flex-1' : ''} {dragType ? 'py-1 -my-1' : ''}"
                    role="list"
                    use:touchDrag={dragType ? { item: '[data-preset-item]', ghost: '[data-preset-row]' } : null}
                    ondragleave={(event) => {
                        if (!(event.relatedTarget instanceof Node && event.currentTarget.contains(event.relatedTarget))) dropAt = null
                    }}
                >
                    {#each visible as { preset, index } (index)}
                        {@const isSelected = selected?.(preset, index) ?? false}
                        <div
                            class="relative border rounded-md transition-colors {openMenu === index
                                ? 'border-selected'
                                : 'border-darkborderc'}"
                            role="listitem"
                            data-preset-item
                            ondragstart={(event) => dragStart(event, preset, index)}
                            ondragend={endDrag}
                            ondragenter={(event) => {
                                if (accepts(event)) event.preventDefault()
                            }}
                            ondragover={(event) => dragOver(event, index)}
                            ondrop={drop}
                        >
                            {#if dropAt === index && movesTo(index)}
                                <span class="{dropLine} -top-[3px]" aria-hidden="true"></span>
                            {:else if dropAt === index + 1 && index === presets.length - 1 && movesTo(index + 1)}
                                <span class="{dropLine} -bottom-[3px]" aria-hidden="true"></span>
                            {/if}
                            <div class="flex items-stretch" data-preset-row>
                                {#if sortable}
                                    <span
                                        class="shrink-0 w-7 flex items-center justify-center text-textcolor2 cursor-grab"
                                        aria-hidden="true"
                                        draggable="true"><GripVerticalIcon size={16} /></span
                                    >
                                {/if}
                                <button
                                    class="flex-1 min-w-0 {sortable
                                        ? 'pr-3'
                                        : 'px-3 rounded-l-md'} py-2 {touchTargets ? 'pointer-coarse:py-2.5' : ''} text-left transition-colors hover:bg-selected flex items-center gap-2"
                                    title={rowTitle}
                                    aria-current={isSelected ? 'true' : undefined}
                                    draggable={sortable ? 'true' : undefined}
                                    onclick={() => guarded(() => apply(preset, index))}
                                >
                                    {#if selected && !rowLeading}
                                        <span class="shrink-0 w-4 text-textcolor">
                                            {#if isSelected}<CheckIcon size={16} />{/if}
                                        </span>
                                    {/if}
                                    {@render rowLeading?.(preset, index)}
                                    <span class="min-w-0 flex-1">
                                        {#if subtitle}
                                            <span class="block text-xs text-textcolor2 truncate">{subtitle(preset)}</span>
                                        {/if}
                                        <span class="block truncate {isSelected ? 'font-medium' : ''}">{preset.name}</span>
                                    </span>
                                    {#if rowLeading && isSelected}<CheckIcon size={16} class="shrink-0" />{/if}
                                    {@render rowTrailing?.(preset, index)}
                                </button>
                                <button
                                    class="shrink-0 w-9 {touchTargets ? 'pointer-coarse:w-11' : ''} flex items-center justify-center rounded-r-md transition-colors hover:bg-selected"
                                    title={language.presetActions}
                                    aria-expanded={openMenu === index}
                                    onclick={() => {
                                        openMenu = openMenu === index ? null : index
                                    }}><EllipsisVerticalIcon size={16} /></button
                                >
                            </div>
                            {#if openMenu === index}
                                <div class="flex flex-wrap gap-1 p-2 border-t border-darkborderc">
                                    {#if movable}
                                        <button
                                            class={small}
                                            title={language.movePresetUp}
                                            disabled={index === 0}
                                            onclick={() => move(index, -1)}><ChevronUpIcon size={14} /></button
                                        >
                                        <button
                                            class={small}
                                            title={language.movePresetDown}
                                            disabled={index === presets.length - 1}
                                            onclick={() => move(index, 1)}><ChevronDownIcon size={14} /></button
                                        >
                                    {/if}
                                    {@render rowActions?.(preset, index, guarded, small)}
                                    <button class={small} onclick={() => rename(index)}
                                        ><PencilIcon size={14} />{language.renamePreset}</button
                                    >
                                    {#if actions || copy}
                                        <button class={small} onclick={() => duplicate(index)}
                                            ><CopyIcon size={14} />{language.duplicatePreset}</button
                                        >
                                    {/if}
                                    {@render rowActionsEnd?.(preset, index, guarded, small)}
                                    {#if actions || confirmDelete}
                                        <button
                                            class="{small} text-draculared border-draculared/40 hover:bg-draculared/15"
                                            onclick={() => remove(index)}
                                            ><TrashIcon size={14} />{language.deletePreset}</button
                                        >
                                    {/if}
                                </div>
                            {/if}
                        </div>
                    {/each}
                </div>
            {/if}
            {@render footer?.(guarded, action)}
        </div>
    </div>
    <button class="absolute inset-0 cursor-default" aria-label={language.cancel} onclick={dismiss}></button>
</div>
