<script lang="ts">
    import { onMount, onDestroy } from "svelte";
    import { XIcon, TrashIcon, PencilIcon, BookOpenCheckIcon, BookLockIcon, ArrowRightIcon } from "@lucide/svelte";
    import Chat from "../ChatScreens/Chat.svelte";
    import LiveDisplayParserBoundary from "../ChatScreens/LiveDisplayParserBoundary.svelte";
    import { getCharImage } from "src/ts/characters";
    import { findCharacterbyId, getUserName, getUserIcon } from "src/ts/util";
    import { createSimpleCharacter, bookmarkListOpen, DBState, selectedCharID, ScrollToMessageStore } from "src/ts/stores.svelte";
    import { language } from "src/lang";
    import { alertInput } from "src/ts/alert";
    import {
        acquireCompleteConversation,
        captureSelectedConversationTarget,
        getActiveConversationSession,
        getPersistentDataRuntime,
    } from "src/ts/storage/persistentDataRuntime.svelte";
    import {
        queryChatMessageTargetsByIds,
        removeCapturedBookmark,
        renameCapturedBookmark,
        type CapturedChatMessageTarget,
    } from "src/ts/chatMessageUi";

    const close = () => $bookmarkListOpen = false;
    let chara = $derived(DBState.db.characters[$selectedCharID]);
    const selectedCharacterId = $derived(chara?.chaId ?? "");
    const selectedConversationId = $derived(chara?.chats[chara.chatPage]?.id ?? "");
    const simpleChar = $derived(createSimpleCharacter(chara));

    function captureCurrentChat() {
        const character = DBState.db.characters[$selectedCharID];
        const conversation = character?.chats[character.chatPage];
        return character && conversation ? { character, conversation } : null;
    }

    const chatMessageContext = {
        captureCurrent: captureCurrentChat,
        getCurrentSession: getActiveConversationSession,
        captureSelectedConversationTarget,
        acquirePersistentRevision: (revision: number) =>
            getPersistentDataRuntime().store.acquireRevision(revision),
        acquireCompleteConversation,
    };

    let bookmarkedMessages = $state<any[]>([]);
    let bookmarkQueryGeneration = 0;
    $effect(() => {
        const currentCharacter = chara;
        const chat = currentCharacter?.chats[currentCharacter.chatPage];
        const bookmarkIds = [...(chat?.bookmarks ?? [])];
        const generation = ++bookmarkQueryGeneration;
        if (!currentCharacter || !chat || bookmarkIds.length === 0) {
            bookmarkedMessages = [];
            return;
        }
        void queryChatMessageTargetsByIds(
            chatMessageContext,
            bookmarkIds,
            'last',
        ).then((targets) => {
            if (generation !== bookmarkQueryGeneration) return;
            bookmarkedMessages = targets.map(target => {
                const message = target.message;
                let speaker = null;
                if (currentCharacter.type === 'group' && message.saying) {
                    speaker = findCharacterbyId(message.saying);
                }
                
                return {
                    ...message,
                    originalIndex: target.absoluteIndex,
                    saying: message.saying ?? '',
                    speaker,
                    target,
                };
            });
        }).catch(() => {
            if (generation === bookmarkQueryGeneration) bookmarkedMessages = [];
        });
    });

    let expandedBookmarks = $state(new Set<string>());
    let expandAll = $state(false);

    onMount(() => {
        const handleKeydown = (event: KeyboardEvent) => {
            if (event.key === 'Escape') {
                close();
            }
        };
        window.addEventListener('keydown', handleKeydown);
        return () => {
            window.removeEventListener('keydown', handleKeydown);
        };
    });

    function toggleExpand(chatId: string) {
        if (expandAll) {
            expandAll = false;
            const allIds = bookmarkedMessages.map(m => m.chatId);
            const newSet = new Set(allIds);
            newSet.delete(chatId);
            expandedBookmarks = newSet;
        } else {
            const newSet = new Set(expandedBookmarks);
            if (newSet.has(chatId)) {
                newSet.delete(chatId);
            } else {
                newSet.add(chatId);
            }
            expandedBookmarks = newSet;
        }
    }

    function toggleExpandAll() {
        expandAll = !expandAll;
        if (expandAll) {
            expandedBookmarks.clear();
        }
    }

    async function editName(target: CapturedChatMessageTarget) {
        await renameCapturedBookmark(target, chatMessageContext, (currentName) =>
            alertInput(language.bookmarkAskNameOrCancel, [], currentName),
        );
    }

    async function removeBookmark(target: CapturedChatMessageTarget) {
        await removeCapturedBookmark(target, chatMessageContext);
    }

    function goToChat(target: CapturedChatMessageTarget) {
        ScrollToMessageStore.set(target);
        close();
    }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
    class="fixed top-0 left-0 w-full h-full z-30 bg-black/50 flex justify-center items-center"
    onclick={(event) => {
        if (event.target === event.currentTarget) {
            close();
        }
    }}
    onkeydown={(event) => {
        if (event.target === event.currentTarget && (event.key === 'Enter' || event.key === ' ')) {
            close();
        }
    }}
>
    <div class="bg-darkbg p-3 rounded-md flex flex-col max-w-4xl w-full max-h-[90%] overflow-y-auto">
        <div class="flex items-center text-textcolor mb-4">
            <h2 class="text-xl font-bold">{language.bookmarks}</h2>
            <div class="ml-auto flex items-center gap-2">
                <button 
                    class="text-textcolor2 hover:text-green-500" 
                    onclick={toggleExpandAll}
                    title={expandAll ? language.collapseAll : language.expandAll}
                >
                    {#if expandAll}
                        <BookLockIcon size={20} />
                    {:else}
                        <BookOpenCheckIcon size={20} />
                    {/if}   
                </button>
                <button class="text-textcolor2 hover:text-green-500" onclick={close}>
                    <XIcon size={24}/>
                </button>
            </div>
        </div>
        
        {#if bookmarkedMessages.length === 0}
            <p class="text-textcolor2">{language.noBookmarks}</p>
        {:else}
            <div class="flex flex-col gap-2">
                {#each bookmarkedMessages as msg (msg.chatId)}
                    <div class="border border-darkborderc rounded-lg">
                        <div 
                            class="flex items-center p-3 cursor-pointer hover:bg-selected transition-colors"
                            onclick={() => toggleExpand(msg.chatId)}
                            onkeydown={(e) => e.key === 'Enter' && toggleExpand(msg.chatId)}
                            role="button"
                            tabindex="0"
                        >
                            <span class="grow text-left truncate">{chara.chats[chara.chatPage].bookmarkNames?.[msg.chatId] || msg.data.substring(0, 30) + '...'}</span>
                            <div class="shrink-0 flex items-center gap-2 ml-2">
                                <button class="text-textcolor2 hover:text-blue-500" title={language.goToChat} onclick={(e) => { e.stopPropagation(); if (msg.target) goToChat(msg.target); }}>
                                    <ArrowRightIcon size={20} />
                                </button>
                                <button class="text-textcolor2 hover:text-green-500" onclick={(e) => { e.stopPropagation(); if (msg.target) editName(msg.target); }}>
                                    <PencilIcon size={16} />
                                </button>
                                <button class="text-textcolor2 hover:text-red-500" onclick={(e) => { e.stopPropagation(); if (msg.target) void removeBookmark(msg.target); }}>
                                    <TrashIcon size={16} />
                                </button>
                            </div>
                        </div>

                        {#if (expandAll || expandedBookmarks.has(msg.chatId)) && msg.target.character.chaId === selectedCharacterId && msg.target.conversation.id === selectedConversationId}
                            <div class="p-1 border-t border-darkborderc">
                                {#key selectedCharacterId + "/" + selectedConversationId + "/" + msg.chatId}
                                    <LiveDisplayParserBoundary
                                        source={msg.data}
                                        character={chara.type === "group" ? (msg.speaker ?? msg.saying) : chara}
                                    >
                                        {#snippet children(signal)}
                                            {#if chara.type === "group"}
                                                <Chat
                                                    parserAbortSignal={signal}
                                                    idx={msg.originalIndex}
                                                    message={msg.data}
                                                    name={msg.speaker?.name}
                                                    img={getCharImage(msg.speaker?.image, "css")}
                                                    role={msg.role}
                                                    messageGenerationInfo={msg.generationInfo}
                                                    rerollIcon={false}
                                                    largePortrait={msg.speaker?.largePortrait}
                                                    character={msg.saying}
                                                    isLastMemory={false}
                                                />
                                            {:else}
                                                <Chat
                                                    parserAbortSignal={signal}
                                                    idx={msg.originalIndex}
                                                    message={msg.data}
                                                    name={msg.role === "user" ? getUserName() : chara.name}
                                                    img={msg.role === "user"
                                                        ? getCharImage(getUserIcon(), "css")
                                                        : getCharImage(chara.image, "css")}
                                                    role={msg.role}
                                                    messageGenerationInfo={msg.generationInfo}
                                                    rerollIcon={false}
                                                    largePortrait={chara.largePortrait}
                                                    character={simpleChar}
                                                    isLastMemory={false}
                                                />
                                            {/if}
                                        {/snippet}
                                    </LiveDisplayParserBoundary>
                                {/key}
                            </div>
                        {/if}
                    </div>
                {/each}
            </div>
        {/if}
    </div>
</div>
