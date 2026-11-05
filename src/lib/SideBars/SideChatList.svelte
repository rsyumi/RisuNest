<script lang="ts">
    import { tick } from "svelte";
    import { v4 } from "uuid";
    import type Sortable from 'sortablejs/modular/sortable.core.esm.js';
    import { DownloadIcon, PencilIcon, HardDriveUploadIcon, MenuIcon, TrashIcon, SplitIcon, FolderPlusIcon, BookmarkCheckIcon } from "@lucide/svelte";

    import type { character } from "src/ts/storage/database.svelte";
    import { DBState } from 'src/ts/stores.svelte';
    import { selectedCharID } from "src/ts/stores.svelte";

    import CheckInput from "../UI/GUI/CheckInput.svelte";
    import Button from "../UI/GUI/Button.svelte";
    import TextInput from "../UI/GUI/TextInput.svelte";

    import { addNewChat, duplicateChat, editSelectedChatList as applyChatListEdit, exportChat, importChat, exportAllChats, removeChat } from "src/ts/characters";
    import { alertChatOptions, alertConfirm, alertError, alertNormal, alertSelect, alertStore } from "src/ts/alert";
    import { sortableOptions } from "src/ts/util";

    import { bookmarkListOpen } from "src/ts/stores.svelte";
    import { language } from "src/lang";
    import { bindPersona, chatBindingBlockedByGeneration, saveChatBinding } from 'src/ts/chatBindings.svelte'
    import Toggles from "./Toggles.svelte";
    import ModuleScopeButton from "./ModuleScopeButton.svelte";
    import { changeChatTo } from "src/ts/globalApi.svelte";
    import { isWorkingSetCharacterStub } from "src/ts/storage/workingSetCatalog";
    import { indexSideChatListRows, orderChatsByDroppedRows, orderFoldersByDroppedIds, type DroppedChatRow } from "./sideChatListRows";

    interface Props {
        chara: character;
    }

    let { chara = $bindable() }: Props = $props();
    let editMode = $state(false)
    let indexedRows = $derived(
        isWorkingSetCharacterStub(chara)
            ? { folders: [], ungrouped: [] }
            : indexSideChatListRows(chara.chats, chara.chatFolders),
    )

    let chatsStb: Sortable[] = []
    let folderStb: Sortable | null = null

    let folderEles: HTMLDivElement = $state()
    let listEle: HTMLDivElement = $state()
    let sorted = $state(0)
    let opened = 0
    let sortableLoadId = 0

    async function editSelectedChatList(...args: Parameters<typeof applyChatListEdit>): Promise<boolean> {
        try {
            return await applyChatListEdit(...args)
        } catch (error) {
            alertError(error)
            return false
        }
    }

    function editFolder(characterId: string, folderId: string, update: (folder: NonNullable<character['chatFolders']>[number]) => void) {
        return editSelectedChatList(characterId, 'edit-chat-folder', (character) => {
            const folder = character.chatFolders?.find((candidate) => candidate.id === folderId)
            if (!folder) return false
            update(folder)
            return null
        })
    }

    function renameChat(characterId: string, chatId: string, name: string) {
        return editSelectedChatList(characterId, 'rename-chat', (character) => {
            const chat = character.chats.find((candidate) => candidate.id === chatId)
            if (!chat) return false
            chat.name = name
            return null
        })
    }

    const destroySortable = () => {
        if (folderStb) {
            try {
                folderStb.destroy()
            } catch {}
            folderStb = null
        }
        for (const sortable of chatsStb) {
            try {
                sortable.destroy()
            } catch {}
        }
        chatsStb = []
    }

    const createStb = async () => {
        const loadId = ++sortableLoadId
        destroySortable()

        await tick()
        const { default: Sortable } =
            await import('sortablejs/modular/sortable.core.esm.js')
        if (loadId !== sortableLoadId || !listEle || !folderEles)
            return

        for (let chat of listEle.querySelectorAll('.risu-chat')) {
            chatsStb.push(new Sortable(chat, {
                group: 'chats',
                onEnd: async () => {
                    // Read the dropped order before re-rendering the list.
                    const rows: DroppedChatRow[] = []
                    listEle.querySelectorAll('[data-risu-chat-folder-idx]').forEach(folder => {
                        const folderIdx = parseInt(folder.getAttribute('data-risu-chat-folder-idx'))
                        folder.querySelectorAll('[data-risu-chat-idx]').forEach(chatInFolder => {
                            const chatIdx = parseInt(chatInFolder.getAttribute('data-risu-chat-idx'))
                            rows.push({ id: chara.chats[chatIdx].id, folderId: chara.chatFolders[folderIdx].id })
                        })
                    })

                    const placed = new Set(rows.map((row) => row.id))
                    listEle.querySelectorAll('[data-risu-chat-idx]').forEach(chatEle => {
                        const id = chara.chats[parseInt(chatEle.getAttribute('data-risu-chat-idx'))].id
                        if (!placed.has(id)) {
                            placed.add(id)
                            rows.push({ id, folderId: null })
                        }
                    })

                    destroySortable()
                    try {
                        await editSelectedChatList(chara.chaId, 'reorder-chats', (character) => {
                            const chats = orderChatsByDroppedRows(character.chats, rows)
                            if (!chats) return false
                            character.chats = chats
                            return null
                        })
                    } finally {
                        sorted += 1
                    }
                },
                ...sortableOptions
            }))
        }
        folderStb = Sortable.create(folderEles, {
            group: 'folders',
            onEnd: async (event) => {
                // Read the dropped order before re-rendering the list.
                const folderIds: string[] = []
                const rows: DroppedChatRow[] = []
                const folders: HTMLElement[] = Array.from<HTMLElement>(event.to.children)

                folders.forEach(folder => {
                    const folderIdx = parseInt(folder.getAttribute('data-risu-chat-folder-idx'))
                    folderIds.push(chara.chatFolders[folderIdx].id)

                    folder.querySelectorAll('[data-risu-chat-idx]').forEach(chatEle => {
                        const idx = parseInt(chatEle.getAttribute('data-risu-chat-idx'))
                        rows.push({ id: chara.chats[idx].id })
                    })
                })

                const placed = new Set(rows.map((row) => row.id))
                listEle.querySelectorAll('[data-risu-chat-idx]').forEach(chatEle => {
                    const id = chara.chats[parseInt(chatEle.getAttribute('data-risu-chat-idx'))].id
                    if (!placed.has(id)) {
                        placed.add(id)
                        rows.push({ id })
                    }
                })

                destroySortable()
                try {
                    await editSelectedChatList(chara.chaId, 'reorder-chat-folders', (character) => {
                        const chatFolders = orderFoldersByDroppedIds(character.chatFolders, folderIds)
                        const chats = orderChatsByDroppedRows(character.chats, rows)
                        if (!chatFolders || !chats) return false
                        character.chatFolders = chatFolders
                        character.chats = chats
                        return null
                    })
                } finally {
                    sorted += 1
                }
            },
            ...sortableOptions
        })
    }

    $effect(() => {
        sorted
        chara.chatFolders?.length
        chara.chats.length
        void createStb()
        return () => {
            sortableLoadId += 1
            destroySortable()
        }
    })
</script>
<div class="flex flex-col w-full shrink-0">
    <Button className="relative bottom-2" onclick={async () => {
        await addNewChat(chara)
    }}>{language.newChat}</Button>

    {#key sorted}
    <div class="flex flex-col mt-2 overflow-y-auto max-h-80 shrink-0" bind:this={listEle}>
        <!-- folder div -->
        <div class="flex flex-col" bind:this={folderEles}>
            <!-- chat folder -->
            {#each indexedRows.folders as folderRow (folderRow.folder.id)}
            {@const folder = folderRow.folder}
            {@const i = folderRow.index}
            <div data-risu-chat-folder-idx={i}
                class="flex flex-col mb-2 border-solid border-1 border-darkborderc cursor-pointer rounded-md">
                <!-- folder header -->
                <button 
                    onclick={() => {
                        if(!editMode) {
                            void editFolder(chara.chaId, folder.id, (current) => { current.folded = !current.folded })
                        }
                    }}
                    class="flex items-center text-textcolor border-solid border-0 border-darkborderc p-2 cursor-pointer rounded-md"
                    class:bg-red-900={folder.color === 'red'}
                    class:bg-yellow-900={folder.color === 'yellow'}
                    class:bg-green-900={folder.color === 'green'}
                    class:bg-blue-900={folder.color === 'blue'}
                    class:bg-indigo-900={folder.color === 'indigo'}
                    class:bg-purple-900={folder.color === 'purple'}
                    class:bg-pink-900={folder.color === 'pink'}
                >
                    {#if editMode}
                        <TextInput value={folder.name} onchange={(e) => {
                            const name = e.currentTarget.value
                            void editFolder(chara.chaId, folder.id, (current) => { current.name = name })
                        }} className="grow min-w-0" padding={false}/>
                    {:else}
                        <span>{folder.name}</span>
                    {/if}
                    <div class="grow flex justify-end">
                        <div role="button" tabindex="0" onkeydown={(e) => {
                            if(e.key === 'Enter'){
                                e.currentTarget.click()
                            }
                        }} class="text-textcolor2 hover:text-green-500 mr-1 cursor-pointer" onclick={async (e) => {
                            e.stopPropagation()
                            const characterId = chara.chaId
                            const folderId = folder.id
                            const sel = parseInt(await alertSelect([language.changeFolderColor, language.cancel]))
                            switch (sel) {
                                case 0:
                                    const colors = ["red","green","blue","yellow","indigo","purple","pink","default"]
                                    const sel = parseInt(await alertSelect(colors))
                                    if (Number.isInteger(sel) && sel >= 0 && sel < colors.length) {
                                        await editFolder(characterId, folderId, (current) => { current.color = colors[sel] })
                                    }
                                    break
                            }
                        }}>
                            <MenuIcon size={18}/>
                        </div>
                        <div role="button" tabindex="0" onkeydown={(e) => {
                            if(e.key === 'Enter'){
                                e.currentTarget.click()
                            }
                        }} class="text-textcolor2 hover:text-green-500 mr-1 cursor-pointer" onclick={(e) => {
                            e.stopPropagation()
                            editMode = !editMode
                        }}>
                            <PencilIcon size={18}/>
                        </div>
                        <div role="button" tabindex="0" onkeydown={(e) => {
                            if(e.key === 'Enter'){
                                e.currentTarget.click()
                            }
                        }} class="text-textcolor2 hover:text-green-500 cursor-pointer" onclick={async (e) => {
                            e.stopPropagation()
                            const characterId = chara.chaId
                            const folderId = folder.id
                            const d = await alertConfirm(`${language.removeConfirm}${folder.name}`)
                            if (d) {
                                await editSelectedChatList(characterId, 'remove-chat-folder', (character) => {
                                    const index = character.chatFolders?.findIndex((candidate) => candidate.id === folderId) ?? -1
                                    if (index < 0) return false
                                    character.chatFolders.splice(index, 1)
                                    for (const chat of character.chats) if (chat.folderId === folderId) chat.folderId = null
                                    return null
                                })
                            }
                        }}>
                            <TrashIcon size={18}/>
                        </div>
                    </div>
                </button>
                <!-- chats in folder -->
                <div class="risu-chat flex flex-col w-full text-textcolor border-solid border-0 border-darkborderc p-2 cursor-pointer rounded-md {folder.folded ? 'hidden' : ''}">
                    {#if folderRow.chats.length === 0}
                    <span class="no-sort flex justify-center text-textcolor2">Empty</span>
                    <div></div>
                    {:else}
                    {#each folderRow.chats as chatRow (chatRow.chat.id)}
                    {@const chat = chatRow.chat}
                    {@const chatIndex = chatRow.index}
                    <button data-risu-chat-idx={chatIndex} onclick={async () => {
                        if(!editMode){
                            await changeChatTo(chat.id)
                        }
                    }} class="risu-chats flex items-center text-textcolor border-solid border-0 border-darkborderc p-2 cursor-pointer rounded-md"class:bg-selected={chatIndex === chara.chatPage}>
                        {#if editMode}
                            <TextInput value={chat.name} onchange={(e) => renameChat(chara.chaId, chat.id, e.currentTarget.value)} className="grow min-w-0" padding={false}/>
                        {:else}
                            <span>{chat.name}</span>
                        {/if}
                        <div class="grow flex justify-end">
                            <div role="button" tabindex="0" onkeydown={(e) => {
                                if(e.key === 'Enter'){
                                    e.currentTarget.click()
                                }
                            }} class="text-textcolor2 hover:text-green-500 mr-1 cursor-pointer" onclick={async (e) => {
                                e.stopPropagation()
                                const option = await alertChatOptions()
                                switch(option){
                                    case 0:{
                                        await duplicateChat(chara.chaId, chat.id)
                                        break
                                    }
                                    case 1:{
                                        if(chatBindingBlockedByGeneration()) break
                                        if(chat.bindedPersona){
                                            const confirm = await alertConfirm(language.doYouWantToUnbindCurrentPersona)
                                            if(confirm){
                                                await bindPersona(chat, -1)
                                                await saveChatBinding()
                                                alertNormal(language.personaUnbindedSuccess)
                                            }
                                        }
                                        else{
                                            const confirm = await alertConfirm(language.doYouWantToBindCurrentPersona)
                                            if(confirm){
                                                await bindPersona(chat, DBState.db.selectedPersona)
                                                await saveChatBinding()
                                                alertNormal(language.personaBindedSuccess)
                                            }
                                        }
                                        break
                                    }
                                }
                            }}>
                                <MenuIcon size={18}/>
                            </div>
                            <div role="button" tabindex="0" onkeydown={(e) => {
                                if(e.key === 'Enter'){
                                    e.currentTarget.click()
                                }
                            }} class="text-textcolor2 hover:text-green-500 mr-1 cursor-pointer" onclick={(e) => {
                                e.stopPropagation()
                                editMode = !editMode
                            }}>
                                <PencilIcon size={18}/>
                            </div>
                            <div role="button" tabindex="0" onkeydown={(e) => {
                                if(e.key === 'Enter'){
                                    e.currentTarget.click()
                                }
                            }} class="text-textcolor2 hover:text-green-500 mr-1 cursor-pointer" onclick={async (e) => {
                                e.stopPropagation()
                                exportChat(chatIndex)
                            }}>
                                <DownloadIcon size={18}/>
                            </div>
                            <div role="button" tabindex="0" onkeydown={(e) => {
                                if(e.key === 'Enter'){
                                    e.currentTarget.click()
                                }
                            }} class="text-textcolor2 hover:text-green-500 cursor-pointer" onclick={async (e) => {
                                e.stopPropagation()
                                if(chara.chats.length === 1){
                                    alertError(language.errors.onlyOneChat)
                                    return
                                }
                                const d = await alertConfirm(`${language.removeConfirm}${chat.name}`)
                                if(d){
                                    await removeChat(chara, chat.id)
                                }
                            }}>
                                <TrashIcon size={18}/>
                            </div>
                        </div>
                    </button>
                    {/each}
                    {/if}
                </div>
            </div>
            {/each}
        </div>
        <!-- chat without folder div -->
        <div class="risu-chat flex flex-col">
            {#each indexedRows.ungrouped as chatRow (chatRow.chat.id)}
            {@const chat = chatRow.chat}
            {@const i = chatRow.index}
            <button data-risu-chat-idx={i} onclick={async () => {
                if(!editMode){
                    await changeChatTo(chat.id)
                }
            }}
            class="flex items-center text-textcolor border-solid border-0 border-darkborderc p-2 cursor-pointer rounded-md"
            class:bg-selected={i === chara.chatPage}>
                {#if editMode}
                    <TextInput value={chat.name} onchange={(e) => renameChat(chara.chaId, chat.id, e.currentTarget.value)} className="grow min-w-0" padding={false}/>
                {:else}
                    <span>{chat.name}</span>
                {/if}
                <div class="grow flex justify-end">
                    <div role="button" tabindex="0" onkeydown={(e) => {
                        if(e.key === 'Enter'){
                            e.currentTarget.click()
                        }
                    }} class="text-textcolor2 hover:text-green-500 mr-1 cursor-pointer" onclick={async (e) => {
                        e.stopPropagation()
                        const option = await alertChatOptions()
                        switch(option){
                            case 0:{
                                await duplicateChat(chara.chaId, chat.id)
                                break
                            }
                            case 1:{
                                const chat = chara.chats[i]
                                if(chatBindingBlockedByGeneration()) break
                                if(chat.bindedPersona){
                                    const confirm = await alertConfirm(language.doYouWantToUnbindCurrentPersona)
                                    if(confirm){
                                        await bindPersona(chat, -1)
                                                await saveChatBinding()
                                        alertNormal(language.personaUnbindedSuccess)
                                    }
                                }
                                else{
                                    const confirm = await alertConfirm(language.doYouWantToBindCurrentPersona)
                                    if(confirm){
                                        await bindPersona(chat, DBState.db.selectedPersona)
                                                await saveChatBinding()
                                        alertNormal(language.personaBindedSuccess)
                                    }
                                }
                                break
                            }
                        }
                    }}>
                        <MenuIcon size={18}/>
                    </div>
                    <div role="button" tabindex="0" onkeydown={(e) => {
                        if(e.key === 'Enter'){
                            e.currentTarget.click()
                        }
                    }} class="text-textcolor2 hover:text-green-500 mr-1 cursor-pointer" onclick={(e) => {
                        e.stopPropagation()
                        editMode = !editMode
                    }}>
                        <PencilIcon size={18}/>
                    </div>
                    <div role="button" tabindex="0" onkeydown={(e) => {
                        if(e.key === 'Enter'){
                            e.currentTarget.click()
                        }
                    }} class="text-textcolor2 hover:text-green-500 mr-1 cursor-pointer" onclick={async (e) => {
                        e.stopPropagation()
                        exportChat(i)
                    }}>
                        <DownloadIcon size={18}/>
                    </div>
                    <div role="button" tabindex="0" onkeydown={(e) => {
                        if(e.key === 'Enter'){
                            e.currentTarget.click()
                        }
                    }} class="text-textcolor2 hover:text-green-500 cursor-pointer" onclick={async (e) => {
                        e.stopPropagation()
                        if(chara.chats.length === 1){
                            alertError(language.errors.onlyOneChat)
                            return
                        }
                        const d = await alertConfirm(`${language.removeConfirm}${chat.name}`)
                        if(d){
                            await removeChat(chara, chat.id)
                        }
                    }}>
                        <TrashIcon size={18}/>
                    </div>
                </div>
            </button>
            {/each}
        </div>
    </div>
    {/key}

    <div class="border-t border-selected mt-2">
        <div class="flex mt-2 ml-2 items-center">
            <button class="text-textcolor2 hover:text-green-500 mr-2 cursor-pointer" onclick={() => {
                exportAllChats()
            }}>
                <DownloadIcon size={18}/>
            </button>
            <button class="text-textcolor2 hover:text-green-500 mr-2 cursor-pointer" onclick={() => {
                importChat()
            }}>
                <HardDriveUploadIcon size={18}/>
            </button>
            <button class="text-textcolor2 hover:text-green-500 mr-2 cursor-pointer" onclick={() => {
                editMode = !editMode
            }}>
                <PencilIcon size={18}/>
            </button>
            <button class="text-textcolor2 hover:text-green-500 mr-2 cursor-pointer" onclick={() => {
                alertStore.set({
                  type: "branches",
                  msg: chara.chaId
                })
            }}>
                <SplitIcon size={18}/>
            </button>
            <button class="text-textcolor2 hover:text-green-500 mr-2 cursor-pointer" onclick={() => {
                $bookmarkListOpen = true;
            }}>
                <BookmarkCheckIcon size={18}/>
            </button>
            <button class="ml-auto text-textcolor2 hover:text-green-500 mr-2 cursor-pointer" onclick={() => {
                void editSelectedChatList(chara.chaId, 'create-chat-folder', (character) => {
                    character.chatFolders ??= []
                    character.chatFolders.unshift({ id: v4(), name: `New Folder ${character.chatFolders.length + 1}`, folded: false })
                    return null
                })
            }}>
                <FolderPlusIcon size={18}/>
            </button>
        </div>

        {#if DBState.db.characters[$selectedCharID]?.chaId !== '§playground'}            
            <Toggles bind:chara={chara} noContainer />
            <ModuleScopeButton />
        {/if}
    </div>

</div>
