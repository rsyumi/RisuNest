<script lang="ts">
    import ListPager from "src/lib/UI/GUI/ListPager.svelte"
    import { loreListWindow, loreDropIndex, groupLoreFolders } from "src/ts/gui/loreListWindow"
    import { type loreBook } from "src/ts/storage/database.svelte";
    import { DBState } from 'src/ts/stores.svelte';
    import LoreBookData from "./LoreBookData.svelte";
    import { selectedCharID } from "src/ts/stores.svelte";
    import Sortable from 'sortablejs/modular/sortable.core.esm.js';
    import { onDestroy, onMount, tick } from "svelte";
    import { sleep, sortableOptions } from "src/ts/util";
    import { v4 } from "uuid";
    import { alertError } from "src/ts/alert";

    let reinitializeSortable = false;

    interface Props {
        globalMode?: boolean;
        submenu?: number;
        lorePlus?: boolean;
        externalLoreBooks?: loreBook[];
        showFolder?: string
    }

    let { globalMode = false, submenu = 0, lorePlus = false, externalLoreBooks = null, showFolder = '' }: Props = $props();
    let page = $state(0)
    const currentItems = $derived(externalLoreBooks ?? (globalMode
        ? DBState.db.loreBook[DBState.db.loreBookPage]?.data ?? []
        : submenu === 1
            ? DBState.db.characters[$selectedCharID]?.chats[DBState.db.characters[$selectedCharID].chatPage]?.localLore ?? []
            : DBState.db.characters[$selectedCharID]?.globalLore ?? []))
    const windowed = $derived(loreListWindow(currentItems, showFolder, page))
    const pageRows = $derived(windowed.rows)
    let stb: Sortable = null
    let ele: HTMLDivElement = $state()
    let sorted = $state(0)
    let idgroup = 'a' + v4() //make should it starts with alphabetic character
    
    // DOM stabilization waiting function
    const waitForDOMReady = async () => {
        // 1. Wait for Svelte tick - component state update completion
        await tick();
        
        // 2. Wait for next frame - DOM rendering completion
        await new Promise(resolve => requestAnimationFrame(resolve));
        
        // 3. Element validity verification
        if (!ele || !ele.isConnected) {
            await new Promise(resolve => setTimeout(resolve, 50));
            if (!ele || !ele.isConnected) {
                throw new Error('Container element is not ready');
            }
        }
        
        // 4. Calculate expected number of child elements
        const expectedElements = pageRows.length;

        // 5. Wait until all child elements are rendered (max 200ms)
        let attempts = 0;
        const maxAttempts = 20;
        while (ele.children.length < expectedElements && attempts < maxAttempts) {
            await new Promise(resolve => setTimeout(resolve, 10));
            attempts++;
        }
        
        // 6. Final stabilization wait (short time)
        await new Promise(resolve => setTimeout(resolve, 10));
    }
    
    // SortableJS recreation function
    const recreateStb = async () => {
        try {
            stb.destroy()
        } catch (error) {
            // Ignore destroy failure (may already be removed)
        }

        // Svelte reactivity trigger - force re-render {#key} block by changing sorted value
        sorted += 1

        // Wait for DOM stabilization (dynamic measurement)
        try {
            await waitForDOMReady();
        } catch (error) {
            console.warn('DOM stabilization failed:', error);
            // Fallback to short fixed wait
            await sleep(100);
        }
        
        // Reactivate drag function only when lorebook detail is not open
        // (drag should be disabled when detail is open)
        if (openedDetails === 0) {
            try {
                createStb(); // Create new SortableJS instance
            } catch (error) {
                console.error('Failed to recreate sortable:', error);
                // Retry
                await sleep(50);
                try {
                    createStb();
                } catch (retryError) {
                    console.error('Retry failed:', retryError);
                }
            }
        }
    }
    
    const createStb = () => {
        stb = Sortable.create(ele, {
            ...sortableOptions,
            group: 'lorebook',        // Enable cross-container drag
            swapThreshold: 0.9,      // More sensitive drag response
            preventOnFilter: false, // Allow click events on filtered elements
            animation: 150, // Animation
            chosenClass: "risu-chosen-item", // Class for the item being dragged
            ghostClass: "risu-ghost-item",  // Class for the drop placeholder

            onEnd: async (evt) => {
                
                // Basic condition check
                if (!evt.from || !evt.to) {
                    alertError('Error: \'evt.from\' or \'evt.to\' is null');
                    await recreateStb();
                    return;
                }
                
                if (evt.oldIndex === undefined || evt.newIndex === undefined) {
                    alertError('Error: oldIndex or newIndex is undefined');
                    await recreateStb();
                    return;
                }
                
                // Cancel movement
                if (evt.oldIndex === evt.newIndex && evt.from === evt.to) {
                    await recreateStb();
                    return;
                }

                // ===== Stage 1: Revert SortableJS DOM manipulation =====
                // SortableJS automatically manipulates DOM upon drag completion,
                // but Svelte uses data-driven rendering, so DOM manipulation must be invalidated
                const indexOfRow = (node: Element | null): number | null => {
                    const value = node?.getAttribute('data-risu-idx')
                    return value === null || value === undefined ? null : Number(value)
                }
                const backingSource = indexOfRow(evt.item)
                const backingNext = indexOfRow(evt.item.nextElementSibling)
                const backingPrevious = indexOfRow(evt.item.previousElementSibling)
                const originalParent = evt.from;    // Drag start container
                const originalIndex = evt.oldIndex; // Drag start position
                
                // Invalidate DOM manipulation: return item to original position
                // (so Svelte can render correctly after data change)
                if (originalParent && evt.item.parentNode !== originalParent) {
                    const referenceNode = originalParent.children[originalIndex];
                    if (referenceNode) {
                        // Insert before if another element exists at original position
                        originalParent.insertBefore(evt.item, referenceNode);
                    } else {
                        // Append to end if original position was last
                        originalParent.appendChild(evt.item);
                    }
                }

                // ===== Stage 2: Collect drag event information =====
                // Identify source and target folders (using data-show-folder attribute)
                const sourceFolder = evt.from.getAttribute('data-show-folder') || '';
                const targetFolder = evt.to.getAttribute('data-show-folder') || '';
                if (backingSource === null) { await recreateStb(); return }
                const oldIndex = backingSource;
                const newIndex = loreDropIndex(oldIndex, backingNext, backingPrevious, currentItems.length);
                
                // ===== Stage 3: Identify current data array =====
                // Select the correct data array based on component props and state
                let currentArray: loreBook[];
                if (externalLoreBooks) {
                    // Use externally passed lorebook array
                    currentArray = externalLoreBooks;
                } else if (submenu === 1) {
                    // Use local chat lorebook
                    currentArray = DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].localLore;
                } else if (globalMode) {
                    // Use global lorebook
                    currentArray = DBState.db.loreBook[DBState.db.loreBookPage].data;
                } else {
                    // Use character global lorebook (default)
                    currentArray = DBState.db.characters[$selectedCharID].globalLore;
                }

                const sourceIdx = oldIndex; // Store SortableJS provided index
                
                let realSourceIdx = sourceIdx;

                // Abort if invalid index (0 is valid index, so check !== undefined)
                if (realSourceIdx === undefined || realSourceIdx === null || realSourceIdx < 0) return;
                const movedItem = currentArray[realSourceIdx]; // Actual item to move
                if (!movedItem) return;

                // ===== Stage 4: Array reconstruction and data application (improved logic) =====

                // 4-1. Create copies of item to move and array
                const newArray = [...currentArray]; // Copy array
                const updatedMovedItem = { ...movedItem }; // Copy item to move
                let moveFolder = false;

                // 4-2. Change folder property of copied item
                if (sourceFolder !== targetFolder) {
                    if (targetFolder) {
                        updatedMovedItem.folder = targetFolder;

                    } else {
                        delete updatedMovedItem.folder;
                    }
                moveFolder = true;
                }

                // 4-3. Sort item to appropriate position
                let finalNewIndex = newIndex; // Final insertion position


                // 4-3-1. Move item in array using oldIndex and modified finalNewIndex
                // First remove original item from array
                
                newArray.splice(realSourceIdx, 1);
                
                // SortableJS newIndex means final target position, so use without adjustment
                let adjustedFinalIndex = finalNewIndex;
                
                // Only perform range check
                if (adjustedFinalIndex > newArray.length) {
                    adjustedFinalIndex = newArray.length;
                }
                
                // For debugging: output drag and drop information
                /*
                alertError('=== Drag and Drop Debugging Info ===\n' +
                          'finalNewIndex: ' + finalNewIndex + '\n' +
                          'realSourceIdx: ' + realSourceIdx + '\n' +
                          'newArray.length (제거 후): ' + newArray.length + '\n' +
                          'adjustedFinalIndex: ' + adjustedFinalIndex + '\n' +
                          'oldIndex < newIndex: ' + (moveFolder && oldIndex < newIndex));*/

                // Insert updated item at new position
                newArray.splice(adjustedFinalIndex, 0, updatedMovedItem);

                // 4-3-2. Reorganize entire array according to folder structure
                const sortedArray = groupLoreFolders(newArray);

                // Assign final sorted array to newArray
                newArray.splice(0, newArray.length, ...sortedArray);

                // For debugging: output current array and new array
                /*alertErrorWait('=== Drag and Drop Debugging Info ===\n' +
                      'oldIndex: ' + oldIndex + ', newIndex: ' + newIndex + '\n' +
                      'Original Array:\n' + JSON.stringify(currentArray, null, 2) + '\n\n' +
                      'Final Sorted Array:\n' + JSON.stringify(newArray, null, 2));*/

                // 4-4. Apply final changed array to appropriate data store
                if (externalLoreBooks) {
                    // Arrays passed as props must be modified internally to reflect in parent
                    externalLoreBooks.splice(0, externalLoreBooks.length, ...newArray);
                } else if (submenu === 1) {
                    DBState.db.characters[$selectedCharID].chats[DBState.db.characters[$selectedCharID].chatPage].localLore = newArray;
                } else if (globalMode) {
                    DBState.db.loreBook[DBState.db.loreBookPage].data = newArray;
                } else {
                    DBState.db.characters[$selectedCharID].globalLore = newArray;
                }
                
                // ===== Stage 5: Force UI synchronization and SortableJS reinitialization =====
                // Remove existing SortableJS instance (prevent DOM inconsistency due to data change)
                await recreateStb()

            }
        })
    }


    onMount(createStb)

    let openedDetails = 0  // Count only lorebook details (for drag deactivation)
    let openedRefs = $state(new Set()) // Track both folders + lorebooks (for UI state)
    
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
    
    const onOpen = (isDetail: boolean = true, bookRef?: any) => {
        if (isDetail) {
            // Disable drag only when lorebook detail opens
            openedDetails += 1
            if(stb){
                try {
                    stb.destroy()
                } catch (error) {}
            }
    }
        if (bookRef) {
            openedRefs.add(bookRef)
            openedRefs = new Set(openedRefs) // Trigger reactivity
        }
    }
    const onClose = (isDetail: boolean = true, bookRef?: any) => {
        if (isDetail) {
            // Consider reactivating drag only when lorebook detail closes
            openedDetails -= 1
            if(openedDetails === 0){
                createStb()
            }
        }
        if (bookRef) {
            openedRefs.delete(bookRef)
            openedRefs = new Set(openedRefs) // Trigger reactivity
        }
    }

    onDestroy(() => {
        if(stb){
            try {
                stb.destroy()
            } catch (error) {  }
        }
    })
</script>

<ListPager bind:page total={windowed.total} disabled={openedRefs.size > 0} />
{#key sorted}
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
                            if (openedRefs.has(book) && !book.folder) {
                                onClose(true, book)
                            }
                            else if(openedRefs.has(book) && book.folder){
                                onClose(false, book)
                            }
                            
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
                            
                            externalLoreBooks = lore
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
                            if (openedRefs.has(book) && !book.folder) {
                                onClose(true, book)
                            }
                            else if(openedRefs.has(book) && book.folder){
                                onClose(false, book)
                            }
                            
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
                            if (openedRefs.has(book) && !book.folder) {
                                onClose(true, book)
                            }
                            else if(openedRefs.has(book) && book.folder){
                                onClose(false, book)
                            }
                            
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
{/key}
