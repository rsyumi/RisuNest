<script lang="ts">
    import { changeChar, getCharImage, removeChar } from "../../ts/characters";
    import { mutatePersistentCharacterDetail } from "../../ts/storage/persistentDataRuntime.svelte";
    import { DBState } from 'src/ts/stores.svelte';
    import BarIcon from "../SideBars/BarIcon.svelte";
    import { ArchiveIcon, ArchiveRestoreIcon, ArrowLeft, User, Users, SquareMousePointer, TrashIcon, Undo2Icon } from "@lucide/svelte";
    import { selectedCharID } from "../../ts/stores.svelte";
    import TextInput from "../UI/GUI/TextInput.svelte";
    import Button from "../UI/GUI/Button.svelte";
    import { language } from "src/lang";
    import { parseMultilangString } from "src/ts/util";
    import { appendCharacterIdToOrder } from "src/ts/storage/characterOrderMutation";
    import MobileCharacters from "../Mobile/MobileCharacters.svelte";
    import {
        archiveCharacterWithConfirmation,
        archivedAt,
        archivedConversationCount,
        archiveIsAvailable,
        characterIsArchived,
        countBlockedGroupMembers,
        formatArchivedAt,
        restoreArchivedCharacterWithConfirmation,
    } from "src/ts/storage/characterArchive";
    interface Props {
        endGrid?: any;
    }

    let { endGrid = () => {} }: Props = $props();
    let search = $state('')
    let selected = $state(3)
    let catalogRoot: HTMLDivElement | null = $state(null)
    let loadMoreSentinel: HTMLDivElement | null = $state(null)
    let visibleCount = $state(48)

    const pageSize = 48
    const supportsAutomaticLoading = typeof IntersectionObserver !== 'undefined'

    type CatalogCharacter = {
            image:string
            index:number
            type:string,
            name:string
            desc:string
            chaId:string
            archived:boolean
            archivedConversations:number
            archivedAt?:number
            blockedMembers:number
    }

    const archiveStrings = language.risuNest.archive

    function normalizeSearch(value: string) {
        return value.replace(/ /g, '').toLocaleLowerCase()
    }

    let normalizedSearch = $derived(normalizeSearch(search))
    let charactersByTrashState = $derived.by(() => {
        const active: CatalogCharacter[] = []
        const trashed: CatalogCharacter[] = []
        const archived: CatalogCharacter[] = []

        for (let index = 0; index < DBState.db.characters.length; index++) {
            const character = DBState.db.characters[index]
            if (!normalizeSearch(character.name).includes(normalizedSearch))
                continue

            const row = {
                image: character.image,
                index,
                type: character.type,
                name: character.name,
                desc: character.creatorNotes ?? 'No description',
                chaId: character.chaId,
                archived: characterIsArchived(character),
                archivedConversations: archivedConversationCount(character),
                archivedAt: archivedAt(character),
                blockedMembers: character.type === 'group'
                    ? countBlockedGroupMembers(character, DBState.db.characters)
                    : 0,
            }
            if (character.trashTime) {
                trashed.push(row)
            } else {
                active.push(row)
                if (row.archived) archived.push(row)
            }
        }

        return { active, trashed, archived }
    })
    let activeCharacters = $derived(charactersByTrashState.active)
    let currentCharacters = $derived(
        selected === 2
            ? charactersByTrashState.trashed
            : selected === 4
                ? charactersByTrashState.archived
                : activeCharacters,
    )

    async function openCharacter(char: CatalogCharacter) {
        if (char.archived) {
            await restoreArchivedCharacterWithConfirmation(char.chaId)
            return
        }
        changeChar(char.index)
    }
    let visibleCharacters = $derived(currentCharacters.slice(0, visibleCount))
    let hasMore = $derived(visibleCount < currentCharacters.length)

    function revealNextPage() {
        visibleCount = Math.min(
            visibleCount + pageSize,
            currentCharacters.length,
        )
    }

    $effect(() => {
        normalizedSearch
        selected
        visibleCount = pageSize
    })

    let loadMoreObserver: IntersectionObserver | null = null
    $effect(() => {
        visibleCount
        if (
            !supportsAutomaticLoading ||
            !catalogRoot ||
            !loadMoreSentinel ||
            !hasMore
        ) {
            loadMoreObserver?.disconnect()
            loadMoreObserver = null
            return
        }

        loadMoreObserver?.disconnect()
        loadMoreObserver = new IntersectionObserver(
            (entries) => {
                if (entries[0]?.isIntersecting) revealNextPage()
            },
            {
                root: catalogRoot,
                rootMargin: '240px 0px',
                threshold: 0,
            },
        )
        loadMoreObserver.observe(loadMoreSentinel)

        return () => {
            loadMoreObserver?.disconnect()
            loadMoreObserver = null
        }
    })
</script>

<div class="h-full w-full flex justify-center">
    <div
        bind:this={catalogRoot}
        class="h-full p-6 bg-darkbg max-w-full w-2xl flex flex-col overflow-y-auto"
    >
        <div class="mx-4 mb-6 flex flex-col">
            <div class="flex items-center gap-3 mb-2">
                <button 
                    class="flex items-center justify-center p-2 rounded-lg hover:bg-selected transition-colors shrink-0"
                    onclick={() => endGrid()}
                    title="Back"
                >
                    <ArrowLeft size={20} />
                </button>
                <div class="flex-1">
                    <TextInput placeholder="Search" bind:value={search} size="lg" autocomplete="off" fullwidth={true}/>
                </div>
            </div>
            <div class="flex flex-wrap gap-2 mt-2">
                <Button styled={selected === 3 ? 'primary' : 'outlined'} size="sm" onclick={() => {selected = 3}}>
                    {language.simple}
                </Button>
                <Button styled={selected === 0 ? 'primary' : 'outlined'} size="sm" onclick={() => {selected = 0}}>
                    {language.grid}
                </Button>
                <Button styled={selected === 1  ? 'primary' : 'outlined'} size="sm" onclick={() => {selected = 1}}>
                    {language.list}
                </Button>
                {#if archiveIsAvailable()}
                    <Button styled={selected === 4  ? 'primary' : 'outlined'} size="sm" onclick={() => {selected = 4}}>
                        {archiveStrings.tab}
                    </Button>
                {/if}
                <Button styled={selected === 2  ? 'primary' : 'outlined'} size="sm" onclick={() => {selected = 2}}>
                    {language.trash}
                </Button>
                <div class="grow"></div>
                <span class="text-textcolor2 text-sm">
                    {activeCharacters.length} {language.character}
                </span>
            </div>
        </div>
        {#if selected === 0}
            <div class="w-full flex justify-center">
                <div class="flex flex-wrap gap-2 w-full justify-center">
                    {#each visibleCharacters as char (char.chaId)}
                        <div class="flex items-center text-textcolor" class:archived-card={char.archived}>
                            {#if char.image}
                                <BarIcon onClick={() => {openCharacter(char)}} additionalStyle={getCharImage(char.image, 'css')}></BarIcon>
                            {:else}
                                <BarIcon onClick={() => {openCharacter(char)}} additionalStyle={char.index === $selectedCharID ? 'background:var(--risu-theme-selected)' : ''}>
                                    {#if char.type === 'group'}
                                        <Users />
                                    {:else}
                                        <User/>
                                    {/if}
                                </BarIcon>
                            {/if}
                        </div>
                    {/each}
                </div>
            </div>
        {:else if selected === 1}
            {#each visibleCharacters as char (char.chaId)}
                {@const parsedDescription = parseMultilangString(char.desc)}
                <div class="flex p-2 border border-darkborderc rounded-md mb-2" class:archived-card={char.archived}>
                    <BarIcon onClick={() => {openCharacter(char)}} additionalStyle={getCharImage(char.image, 'css')}></BarIcon>
                    <div class="flex-1 flex flex-col ml-2">
                        <h4 class="text-textcolor font-bold text-lg mb-1">{char.name || "Unnamed"}</h4>
                        {#if char.archived}
                            <span class="text-textcolor2">{archiveStrings.listedAs}</span>
                        {:else if char.blockedMembers > 0}
                            <span class="text-textcolor2">
                                {archiveStrings.groupMemberBlocked.replace('{0}', String(char.blockedMembers))}
                            </span>
                        {:else}
                            <span class="text-textcolor2">{parsedDescription['en'] || parsedDescription['xx'] || 'No description'}</span>
                        {/if}
                        <div class="flex gap-2 justify-end items-center">
                            {#if char.archived}
                                <span class="text-textcolor2 text-xs border border-darkborderc rounded-md px-2 py-1">
                                    {archiveStrings.listedBadge}
                                </span>
                                <button class="hover:text-textcolor text-textcolor2" aria-label={archiveStrings.restore} onclick={() => {
                                    restoreArchivedCharacterWithConfirmation(char.chaId)
                                }}>
                                    <ArchiveRestoreIcon />
                                </button>
                            {:else}
                                <button class="hover:text-textcolor text-textcolor2" onclick={() => {
                                    changeChar(char.index)
                                }}>
                                    <SquareMousePointer />
                                </button>
                                {#if archiveIsAvailable() && char.type !== 'group'}
                                    <button class="hover:text-textcolor text-textcolor2" aria-label={archiveStrings.action} onclick={() => {
                                        archiveCharacterWithConfirmation(char.chaId)
                                    }}>
                                        <ArchiveIcon />
                                    </button>
                                {/if}
                            {/if}
                            <button class="hover:text-textcolor text-textcolor2" onclick={() => {
                                removeChar(char.chaId, char.name, char.archived ? 'permanent' : 'normal')
                            }}>
                                <TrashIcon />
                            </button>
                        </div>
                    </div>
                </div>
            {/each}
        {:else if selected === 2}
            <span class="text-textcolor2 text-sm mb-2">{language.trashDesc}</span>
            {#each visibleCharacters as char (char.chaId)}
                {@const parsedDescription = parseMultilangString(char.desc)}
                <div class="flex p-2 border border-darkborderc rounded-md mb-2">
                    <BarIcon onClick={() => {changeChar(char.index)}} additionalStyle={getCharImage(char.image, 'css')}></BarIcon>
                    <div class="flex-1 flex flex-col ml-2">
                        <h4 class="text-textcolor font-bold text-lg mb-1">{char.name || "Unnamed"}</h4>
                        <span class="text-textcolor2">{parsedDescription['en'] || parsedDescription['xx'] || 'No description'}</span>
                        <div class="flex gap-2 justify-end">
                            <button class="hover:text-textcolor text-textcolor2" onclick={async () => {
                                await mutatePersistentCharacterDetail(
                                    char.chaId,
                                    'character-restore',
                                    ({ root, character }) => {
                                        delete character.trashTime
                                        appendCharacterIdToOrder(root, char.chaId)
                                    },
                                )
                            }}>
                                <Undo2Icon />
                            </button>
                            <button class="hover:text-textcolor text-textcolor2" onclick={() => {
                                removeChar(char.chaId, char.name, 'permanent')
                            }}>
                                <TrashIcon />
                            </button>
                        </div>
                    </div>
                </div>
            {/each}
        {:else if selected === 4}
            <span class="text-textcolor2 text-sm mb-2">{archiveStrings.listDescription}</span>
            {#each visibleCharacters as char (char.chaId)}
                <div class="flex p-2 border border-darkborderc rounded-md mb-2 archived-card">
                    <BarIcon onClick={() => {restoreArchivedCharacterWithConfirmation(char.chaId)}} additionalStyle={getCharImage(char.image, 'css')}></BarIcon>
                    <div class="flex-1 flex flex-col ml-2">
                        <h4 class="text-textcolor font-bold text-lg mb-1">{char.name || "Unnamed"}</h4>
                        <span class="text-textcolor2">
                            {archiveStrings.archivedAt
                                .replace('{0}', String(char.archivedConversations))
                                .replace('{1}', formatArchivedAt(char.archivedAt))}
                        </span>
                        <div class="flex gap-2 justify-end">
                            <button class="hover:text-textcolor text-textcolor2" aria-label={archiveStrings.restore} onclick={() => {
                                restoreArchivedCharacterWithConfirmation(char.chaId)
                            }}>
                                <ArchiveRestoreIcon />
                            </button>
                            <button class="hover:text-textcolor text-textcolor2" onclick={() => {
                                removeChar(char.chaId, char.name, 'permanent')
                            }}>
                                <TrashIcon />
                            </button>
                        </div>
                    </div>
                </div>
            {/each}
        {:else if selected === 3}
            <MobileCharacters endGrid={endGrid} search={search} hideTrash={true} />
        {/if}
        {#if selected !== 3 && hasMore}
            <div
                bind:this={loadMoreSentinel}
                data-load-more-sentinel
                class="min-h-12 flex items-center justify-center"
            >
                {#if !supportsAutomaticLoading}
                    <button
                        type="button"
                        class="m-2 px-3 py-1 text-sm rounded-md border border-darkborderc text-textcolor2 hover:bg-selected focus:outline-hidden focus:ring-2 focus:ring-selected"
                        onclick={revealNextPage}
                        aria-label={language.loadMore}
                    >
                        {language.loadMore}
                    </button>
                {/if}
            </div>
        {/if}
    </div>
</div>

<style>
    .archived-card {
        filter: grayscale(1);
        opacity: 0.7;
    }
</style>
