<script module lang="ts">
    import type { loreBook as LoreBookEntry } from "src/ts/storage/database.svelte";

    interface LoreDragSession {
        group: string
        book: LoreBookEntry
        /** The folder row that owns the list the drag started in. */
        folderRow: LoreBookEntry | null
        flipped: boolean
        /** Puts the dragged row back where its list rendered it. */
        restore: () => void
        /** Records the dragged row's position after its list renders a new page. */
        reanchor: () => void
    }

    const pageSize = 60
    const pageFlipDelayMs = 500
    const pageFlipRepeatMs = 700
    let dragSession = $state.raw<LoreDragSession | null>(null)
    let revealRequest = $state.raw<{ group: string; book: LoreBookEntry } | null>(null)
</script>

<script lang="ts">
    import { ChevronDownIcon, ChevronUpIcon } from '@lucide/svelte'
    import { language } from "src/lang"
    import ListPager from "src/lib/UI/GUI/ListPager.svelte"
    import { loreListWindow, loreDropIndex, groupLoreFolders, lorePageOf, dropLoreOnFolder } from "src/ts/gui/loreListWindow"
    import { type loreBook } from "src/ts/storage/database.svelte";
    import { DBState } from 'src/ts/stores.svelte';
    import LoreBookData from "./LoreBookData.svelte";
    import { selectedCharID } from "src/ts/stores.svelte";
    import Sortable from 'sortablejs';
    import { onDestroy, onMount, tick } from "svelte";
    import { sortableOptions } from "src/ts/util";
    import { abandonSortDrag, destroySortableOnClose, watchSortDrag } from "src/ts/sortableOptions";
    import { v4 } from "uuid";

    interface Props {
        globalMode?: boolean;
        submenu?: number;
        lorePlus?: boolean;
        externalLoreBooks?: loreBook[];
        showFolder?: string
        dragGroup?: string
    }

    let { globalMode = false, submenu = 0, lorePlus = false, externalLoreBooks = null, showFolder = '', dragGroup = 'a' + v4() }: Props = $props();
    let page = $state(0)
    const currentItems = $derived(externalLoreBooks ?? (globalMode
        ? DBState.db.loreBook[DBState.db.loreBookPage]?.data ?? []
        : submenu === 1
            ? DBState.db.characters[$selectedCharID]?.chats[DBState.db.characters[$selectedCharID].chatPage]?.localLore ?? []
            : DBState.db.characters[$selectedCharID]?.globalLore ?? []))
    const idgroup = $derived(dragGroup)
    let pinEdge: 'start' | 'end' = $state('start')
    const pinned = $derived(dragSession?.group === idgroup
        ? [dragSession.book, dragSession.folderRow].filter(book => book !== null)
        : [])
    const windowed = $derived(loreListWindow(currentItems, showFolder, page, pageSize, pinned, pinEdge))
    const pageRows = $derived(windowed.rows)
    const pageCount = $derived(Math.max(1, Math.ceil(windowed.total / pageSize)))
    const currentPage = $derived(Math.min(page, pageCount - 1))
    let stb: Sortable = null
    let ele: HTMLDivElement = $state()
    let originalNextSibling: Node | null = null
    let ownSession: LoreDragSession | null = null
    let draggedRow: HTMLElement | null = null
    let dragPoint: { x: number, y: number } | null = null
    let dragWatch: ReturnType<typeof watchSortDrag> | null = null
    let headerTarget: HTMLElement | null = null
    let markedRow: Element | null = null
    let markedList: Element | null = null
    const dragMoveEvents = ['pointermove', 'touchmove', 'dragover'] as const

    function markHeaderTarget(header: HTMLElement | null, dimSlot: boolean) {
        const row = header?.closest('[data-risu-idx]') ?? null
        const list = row?.closest('[data-show-folder=""]') ?? null
        if (markedRow !== row) markedRow?.removeAttribute('data-lore-drop-target')
        if (markedList !== list) markedList?.removeAttribute('data-lore-header-drop')
        headerTarget = header
        markedRow = row
        markedList = list
        row?.setAttribute('data-lore-drop-target', '')
        list?.toggleAttribute('data-lore-header-drop', dimSlot)
    }

    function updateHeaderTarget() {
        const draggingEntry = ownSession && ownSession.book.mode !== 'folder'
        const under = draggingEntry && dragPoint ? document.elementFromPoint(dragPoint.x, dragPoint.y) : null
        // A closed folder takes the entry anywhere on its row, an open one on its name row.
        const row = under?.closest('[data-risu-idx]')
        const header = under?.closest<HTMLElement>('[data-lore-folder-header]')
            ?? (row && !row.querySelector('[data-show-folder]') ? row.querySelector<HTMLElement>(':scope > [data-lore-folder-header]') : null)
        if (header?.closest('[data-risu-idgroup]')?.getAttribute('data-risu-idgroup') !== idgroup) {
            markHeaderTarget(null, false)
            return
        }
        // A row dropped on a folder lands first in it, where the slot may already be.
        const slotList = draggedRow?.parentElement
        const slotTopsFolder = slotList?.getAttribute('data-show-folder') === header.getAttribute('data-lore-folder-header')
            && !draggedRow?.previousElementSibling
        markHeaderTarget(header, !slotTopsFolder)
    }

    function trackDragPoint(event: Event) {
        const point = 'touches' in event ? (event as TouchEvent).touches[0] : event as MouseEvent
        if (!point || typeof point.clientX !== 'number') return
        dragPoint = { x: point.clientX, y: point.clientY }
        updateHeaderTarget()
    }

    function startHeaderTracking(row: HTMLElement, original: Event | undefined) {
        draggedRow = row
        dragWatch = watchSortDrag()
        for (const type of dragMoveEvents) document.addEventListener(type, trackDragPoint, { capture: true, passive: true })
        // The list also moves under a resting pointer while it scrolls.
        document.addEventListener('scroll', updateHeaderTarget, { capture: true, passive: true })
        if (original) trackDragPoint(original)
    }

    /** Briefly outlines the folder row that took a dropped entry. */
    function showReceived(row: Element | null) {
        if (!row) return
        const done = (event: Event) => {
            if (event.target !== row) return
            row.removeAttribute('data-lore-received')
            row.removeEventListener('animationend', done)
            row.removeEventListener('animationcancel', done)
        }
        row.addEventListener('animationend', done)
        row.addEventListener('animationcancel', done)
        row.setAttribute('data-lore-received', '')
    }

    /** Ends the tracking and returns the folder header the row was released on. */
    function stopHeaderTracking(): HTMLElement | null {
        for (const type of dragMoveEvents) document.removeEventListener(type, trackDragPoint, true)
        document.removeEventListener('scroll', updateHeaderTarget, true)
        const header = headerTarget
        markHeaderTarget(null, false)
        dragWatch?.stop()
        dragWatch = null
        draggedRow = null
        dragPoint = null
        return header
    }

    const createStb = () => {
        stb = Sortable.create(ele, {
            ...sortableOptions,
            group: idgroup,
            draggable: '> [data-risu-idx]',
            swapThreshold: 0.9,
            preventOnFilter: false,
            animation: 150,
            chosenClass: "risu-chosen-item",
            onMove: (event) => {
                const moved = currentItems[Number(event.dragged.getAttribute('data-risu-idx'))]
                if (moved?.mode === 'folder' && event.from !== event.to) return false
                // Keep the folder row under the pointer while it is the drop target.
                if (headerTarget) return false
                return sortableOptions.onMove(event)
            },
            onStart: (event) => {
                originalNextSibling = event.item.nextSibling
                revealRequest = null
                const book = currentItems[Number(event.item.getAttribute('data-risu-idx'))]
                if (!book) return
                const { item, from } = event
                ownSession = {
                    group: idgroup,
                    book,
                    folderRow: showFolder
                        ? currentItems.find(row => row.mode === 'folder' && row.key === showFolder) ?? null
                        : null,
                    flipped: false,
                    restore: () => from.insertBefore(item, originalNextSibling),
                    reanchor: () => { originalNextSibling = item.nextSibling },
                }
                dragSession = ownSession
                startHeaderTracking(item, (event as Sortable.SortableEvent & { originalEvent?: Event }).originalEvent)
            },
            onChange: updateHeaderTarget,
            onEnd: (event) => {
                const cancelled = dragWatch?.cancelled(event)
                const droppedOn = stopHeaderTracking()
                const session = ownSession
                ownSession = null
                if (session && dragSession === session) dragSession = null
                const indexOfRow = (node: Element | null): number | null => {
                    const value = node?.getAttribute('data-risu-idx')
                    return value === null || value === undefined ? null : Number(value)
                }
                const source = indexOfRow(event.item)
                const next = indexOfRow(event.item.nextElementSibling)
                const previous = indexOfRow(event.item.previousElementSibling)
                // Restore Svelte's DOM before changing either folder's keyed list.
                event.from.insertBefore(event.item, originalNextSibling)
                // An abandoned drag changes nothing.
                if (source === null || cancelled) return
                if (droppedOn) {
                    const moved = currentItems[source]
                    const reordered = dropLoreOnFolder(currentItems, source, droppedOn.getAttribute('data-lore-folder-header'))
                    if (!reordered) return
                    currentItems.splice(0, currentItems.length, ...reordered)
                    revealRequest = { group: idgroup, book: moved }
                    showReceived(droppedOn.closest('[data-risu-idx]'))
                    return
                }
                if (!session?.flipped && event.from === event.to && event.oldIndex === event.newIndex) return

                const moved = currentItems[source]
                if (!moved) return
                const targetFolder = event.to.getAttribute('data-show-folder') || ''
                if (moved.mode === 'folder' && targetFolder) return
                const destination = loreDropIndex(source, next, previous, currentItems.length)
                if (targetFolder) moved.folder = targetFolder
                else delete moved.folder
                const reordered = [...currentItems]
                reordered.splice(source, 1)
                reordered.splice(destination, 0, moved)
                currentItems.splice(0, currentItems.length, ...groupLoreFolders(reordered))
                // A move across a page boundary can shift the row onto a neighbouring page.
                revealRequest = { group: idgroup, book: moved }
            },
        })
    }

    onMount(createStb)

    let openedRefs = $state(new Set<loreBook>())
    
    // Derived state to calculate number of open folders
    let openFolders = $derived(() => {
        let count = 0
        for (const ref of openedRefs) {
            if (ref && typeof ref === 'object' && 'mode' in ref && ref.mode === 'folder') {
                count++
            }
        }
        return count
    })
    
    const onOpen = (isDetail: boolean = true, bookRef?: loreBook) => {
        if (!bookRef || openedRefs.has(bookRef)) return
        if (isDetail && stb) {
            abandonSortDrag()
            stb.destroy()
            stb = null
        }
        openedRefs = new Set([...openedRefs, bookRef])
    }
    const onClose = (isDetail: boolean = true, bookRef?: loreBook) => {
        if (!openedRefs.has(bookRef)) return
        openedRefs.delete(bookRef)
        openedRefs = new Set(openedRefs)
        if (isDetail && ![...openedRefs].some(book => book.mode !== 'folder')) createStb()
    }

    $effect(() => {
        for (const book of openedRefs) {
            if (!pageRows.some(row => row.book === book)) onClose(book.mode !== 'folder', book)
        }
    })

    const hasOpenDetail = $derived([...openedRefs].some(book => book.mode !== 'folder'))
    const dragging = $derived(dragSession?.group === idgroup && !hasOpenDetail && pageCount > 1)
    let previousZone: HTMLDivElement | undefined = $state()
    let nextZone: HTMLDivElement | undefined = $state()
    let hoveredZone: -1 | 0 | 1 = $state(0)
    let flipTimer: ReturnType<typeof setTimeout> | undefined

    async function flipPage(direction: -1 | 1) {
        const session = dragSession
        const target = currentPage + direction
        if (session?.group !== idgroup || target < 0 || target >= pageCount) return
        session.restore()
        session.flipped = true
        pinEdge = direction > 0 ? 'start' : 'end'
        page = target
        await tick()
        if (dragSession === session) session.reanchor()
    }

    function zoneAt(x: number, y: number): -1 | 0 | 1 {
        for (const [zone, direction] of [[previousZone, -1], [nextZone, 1]] as const) {
            const rect = zone?.getBoundingClientRect()
            if (rect && x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom) return direction
        }
        return 0
    }

    function trackPointer(x: number, y: number) {
        const zone = zoneAt(x, y)
        if (zone === hoveredZone) return
        hoveredZone = zone
        clearTimeout(flipTimer)
        if (zone === 0) return
        const flip = () => {
            void flipPage(zone)
            flipTimer = setTimeout(flip, pageFlipRepeatMs)
        }
        flipTimer = setTimeout(flip, pageFlipDelayMs)
    }

    $effect(() => {
        if (!dragging) return
        // Native drags report drag events and touch drags report touch events. Sortable
        // stops dragover over its rows, so listen before the event reaches them.
        const onPointer = (event: DragEvent | PointerEvent) => trackPointer(event.clientX, event.clientY)
        const onTouch = (event: TouchEvent) => {
            const touch = event.touches[0]
            if (touch) trackPointer(touch.clientX, touch.clientY)
        }
        document.addEventListener('dragenter', onPointer, true)
        document.addEventListener('dragover', onPointer, true)
        document.addEventListener('pointermove', onPointer, true)
        document.addEventListener('touchmove', onTouch, { capture: true, passive: true })
        return () => {
            document.removeEventListener('dragenter', onPointer, true)
            document.removeEventListener('dragover', onPointer, true)
            document.removeEventListener('pointermove', onPointer, true)
            document.removeEventListener('touchmove', onTouch, true)
            clearTimeout(flipTimer)
            hoveredZone = 0
        }
    })

    $effect(() => {
        const request = revealRequest
        if (request?.group !== idgroup) return
        const target = lorePageOf(currentItems, showFolder, request.book, pageSize)
        if (target === null) return
        page = target
        revealRequest = null
    })

    onDestroy(() => {
        if (ownSession && dragSession === ownSession) dragSession = null
        if(stb){
            try {
                destroySortableOnClose(stb)
            } catch (error) {  }
        }
    })
</script>

<div class="relative">
    <ListPager bind:page total={windowed.total} disabled={openedRefs.size > 0} />
    {#if dragging && currentPage > 0}
        <div
            bind:this={previousZone}
            aria-hidden="true"
            data-lore-page-zone="previous"
            class="absolute inset-0 flex items-center justify-center gap-2 rounded-md border text-sm transition-colors {hoveredZone === -1 ? 'border-selected bg-selected text-textcolor' : 'border-dashed border-darkborderc bg-darkbg text-textcolor2'}"
        >
            <ChevronUpIcon size={16} />
            <span>{language.risuNest.pager.previous}</span>
            <span class="tabular-nums">{currentPage + 1} / {pageCount}</span>
        </div>
    {/if}
</div>
    <div class="border-solid border-selected p-2 flex flex-col border-1 rounded-md" 
         bind:this={ele} 
         data-show-folder={showFolder || ''}>
        {#if globalMode}
            <!--
                This was a place for global lorebooks, but it was removed :)
            -->
        {:else if externalLoreBooks}
            {@const lastVisibleItem = pageRows.at(-1)?.book}
            {#if externalLoreBooks.length === 0}
                <span class="text-textcolor2">No Lorebook</span>
            {:else}
                {#each pageRows as { book, i } (book)}
                    {#if (!showFolder && !book.folder) || (showFolder === book.folder)}
                        <LoreBookData idgroup={idgroup} bind:value={externalLoreBooks[i]} idx={i} 
                        isOpen={openedRefs.has(book)}
                        openFolders={openFolders()}
                        isLastInContainer={book === lastVisibleItem}
                        onRemove={() => {
                            if (openedRefs.has(book)) onClose(book.mode !== 'folder', book)
                            
                            let lore = externalLoreBooks
                            
                            // When deleting a folder, also delete all items that belong to that folder
                            if (book.mode === 'folder') {
                                // Close items belonging to the folder if they are open
                                lore.forEach(item => {
                                    if (item.folder === book.key && openedRefs.has(item)) {
                                        onClose(true, item)
                                    }
                                })
                                
                                // Filter out the folder and all items belonging to it
                                lore = lore.filter(item => 
                                    item !== book && item.folder !== book.key
                                )
                            } else {
                                // Delete regular item
                                lore.splice(i, 1)
                            }
                            
                            if (lore !== externalLoreBooks) {
                                externalLoreBooks.splice(0, externalLoreBooks.length, ...lore)
                            }
                        }} 
                        onOpen={(isDetail = true) => onOpen(isDetail, book)}
                        onClose={(isDetail = true) => onClose(isDetail, book)}
                        bind:externalLoreBooks={externalLoreBooks} />
                    {/if}
                {/each}
            {/if}
        {:else if submenu === 0}
            {@const lastVisibleItem = pageRows.at(-1)?.book}
            {#if DBState.db.characters[$selectedCharID].globalLore.length === 0}
                <span class="text-textcolor2">No Lorebook</span>
            {:else}
                {#each pageRows as { book, i } (book)}
                    {#if (!showFolder && !book.folder) || (showFolder === book.folder)}
                        <LoreBookData idgroup={idgroup} bind:value={DBState.db.characters[$selectedCharID].globalLore[i]} idx={i} 
                        isOpen={openedRefs.has(book)}
                        openFolders={openFolders()}
                        isLastInContainer={book === lastVisibleItem}
                        onRemove={() => {
                            if (openedRefs.has(book)) onClose(book.mode !== 'folder', book)
                            
                            let lore  = DBState.db.characters[$selectedCharID].globalLore
                            
                            // When deleting a folder, also delete all items that belong to that folder
                            if (book.mode === 'folder') {
                                // Close items belonging to the folder if they are open
                                lore.forEach(item => {
                                    if (item.folder === book.key && openedRefs.has(item)) {
                                        onClose(true, item)
                                    }
                                })
                                
                                // Filter out the folder and all items belonging to it
                                lore = lore.filter(item => 
                                    item !== book && item.folder !== book.key
                                )
                            } else {
                                // Delete regular item
                                lore.splice(i, 1)
                            }
                            
                            DBState.db.characters[$selectedCharID].globalLore = lore
                        }} 
                        onOpen={(isDetail = true) => onOpen(isDetail, book)}
                        onClose={(isDetail = true) => onClose(isDetail, book)}
                        lorePlus={lorePlus} bind:externalLoreBooks={DBState.db.characters[$selectedCharID].globalLore}/>
                    {/if}
                {/each}
            {/if}
        {:else if submenu === 1}
            {@const lastVisibleItem = pageRows.at(-1)?.book}
            {#if DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].localLore.length === 0}
                <span class="text-textcolor2">No Lorebook</span>
            {:else}
                {#each pageRows as { book, i } (book)}
                    {#if (!showFolder && !book.folder) || (showFolder === book.folder)}
                        <LoreBookData idgroup={idgroup} bind:value={DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].localLore[i]} idx={i} 
                        isOpen={openedRefs.has(book)}
                        openFolders={openFolders()}
                        isLastInContainer={book === lastVisibleItem}
                        onRemove={() => {
                            if (openedRefs.has(book)) onClose(book.mode !== 'folder', book)
                            
                            let lore  = DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].localLore
                            
                            // When deleting a folder, also delete all items that belong to that folder
                            if (book.mode === 'folder') {
                                // Close items belonging to the folder if they are open
                                lore.forEach(item => {
                                    if (item.folder === book.key && openedRefs.has(item)) {
                                        onClose(true, item)
                                    }
                                })
                                
                                // Filter out the folder and all items belonging to it
                                lore = lore.filter(item => 
                                    item !== book && item.folder !== book.key
                                )
                            } else {
                                // Delete regular item
                                lore.splice(i, 1)
                            }
                            
                            DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].localLore = lore
                        }} 
                        onOpen={(isDetail = true) => onOpen(isDetail, book)}
                        onClose={(isDetail = true) => onClose(isDetail, book)}
                        lorePlus={lorePlus} bind:externalLoreBooks={DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].localLore}/>
                    {/if}
                {/each}
            {/if}
        {/if}
    </div>
{#if dragging && currentPage + 1 < pageCount}
    <div
        bind:this={nextZone}
        aria-hidden="true"
        data-lore-page-zone="next"
        class="mt-2 h-10 flex items-center justify-center gap-2 rounded-md border text-sm transition-colors {hoveredZone === 1 ? 'border-selected bg-selected text-textcolor' : 'border-dashed border-darkborderc bg-darkbg text-textcolor2'}"
    >
        <ChevronDownIcon size={16} />
        <span>{language.risuNest.pager.next}</span>
        <span class="tabular-nums">{currentPage + 1} / {pageCount}</span>
    </div>
{/if}
