<script lang="ts">
    import { DBState } from 'src/ts/stores.svelte';
    import BarIcon from "../SideBars/BarIcon.svelte";
    import { addCharacter, changeChar, getCharImage } from "src/ts/characters";
    import { characterIsArchived } from "src/ts/storage/characterArchiveView";
    import { restoreArchivedCharacterWithConfirmation } from "src/ts/storage/characterArchive";
    import { MobileSearch } from "src/ts/stores.svelte";
    import { MessageSquareIcon, PlusIcon } from "@lucide/svelte";
    import { getCatalogConversationCount } from "src/ts/storage/workingSetCatalog";
    import { language } from "src/lang";

    interface Props {
        endGrid?: () => void;
        search?: string;
        hideTrash?: boolean;
    }

    const agoFormatter = new Intl.RelativeTimeFormat(navigator.languages, { style: 'short' });

    let {endGrid = () => {}, search, hideTrash = false}: Props = $props();
    let catalogRoot: HTMLDivElement | null = $state(null);
    let loadMoreSentinel: HTMLDivElement | null = $state(null);
    let visibleCount = $state(48);

    const pageSize = 48;
    const supportsAutomaticLoading = typeof IntersectionObserver !== 'undefined';
    let normalizedSearch = $derived(normalizeSearch(search ?? $MobileSearch));

    function normalizeSearch(value:string){
        return value.replace(/ /g,"").toLocaleLowerCase();
    }

    function makeAgoText(time:number){
        if(time === 0){
            return "Unknown";
        }
        const diff = Date.now() - time;
        if(diff < 3600000){
            const min = Math.floor(diff / 60000);
            return agoFormatter.format(-min, 'minute');
        }
        if(diff < 86400000){
            const hour = Math.floor(diff / 3600000);
            return agoFormatter.format(-hour, 'hour');
        }
        if(diff < 604800000){
            const day = Math.floor(diff / 86400000);
            return agoFormatter.format(-day, 'day');
        }
        if(diff < 2592000000){
            const week = Math.floor(diff / 604800000);
            return agoFormatter.format(-week, 'week');
        }
        if(diff < 31536000000){
            const month = Math.floor(diff / 2592000000);
            return agoFormatter.format(-month, 'month');
        }
        const year = Math.floor(diff / 31536000000);
        return agoFormatter.format(-year, 'year');
    }

    let matchingCharacters = $derived.by(() => {
        const rows: Array<{
            chaId: string
            name: string
            image: string
            chats: number
            index: number
            interaction: number
            agoText: string
            archived: boolean
        }> = []
        for (let index = 0; index < DBState.db.characters.length; index += 1) {
            const character = DBState.db.characters[index]
            if (hideTrash && character.trashTime) continue
            const name = character.name || 'Unnamed'
            if (!normalizeSearch(name).includes(normalizedSearch)) continue
            const interaction = character.lastInteraction || 0
            rows.push({
                chaId: character.chaId,
                name,
                image: character.image,
                chats: getCatalogConversationCount(character),
                archived: characterIsArchived(character),
                index,
                interaction,
                agoText: makeAgoText(interaction),
            })
        }
        return rows.sort((left, right) =>
            left.interaction === right.interaction
                ? left.name.localeCompare(right.name)
                : right.interaction - left.interaction,
        )
    })
    let visibleCharacters = $derived(matchingCharacters.slice(0, visibleCount))
    let hasMore = $derived(visibleCount < matchingCharacters.length)

    function revealNextPage() {
        visibleCount = Math.min(
            visibleCount + pageSize,
            matchingCharacters.length,
        )
    }

    $effect(() => {
        normalizedSearch
        hideTrash
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

<div
    bind:this={catalogRoot}
    class="flex flex-col items-center w-full overflow-y-auto h-full"
>
    {#each visibleCharacters as char, index (char.chaId)}
        <button
            data-character-id={char.chaId}
            class="flex p-2 border-t-darkborderc gap-2 w-full"
            class:border-t={index !== 0}
            class:archived-card={char.archived}
            onclick={async () => {
                if(char.archived){
                    await restoreArchivedCharacterWithConfirmation(char.chaId)
                    return
                }
                if(await changeChar(char.index)) endGrid()
            }}
        >
            <BarIcon additionalStyle={getCharImage(char.image, 'css')}></BarIcon>
            <div class="flex flex-1 w-full flex-col justify-start items-start text-start">
                <span>{char.name}</span>
                <div class="text-sm text-textcolor2 flex items-center w-full flex-wrap">
                    {#if char.archived}
                        <span>{language.risuNest.archive.listedAs}</span>
                    {:else}
                        <span class="mr-1">{char.chats}</span>
                        <MessageSquareIcon size={14} />
                        <span class="mr-1 ml-1">|</span>
                        <span>{char.agoText}</span>
                    {/if}
                </div>
            </div>
        </button>
    {/each}
    {#if hasMore}
        <div
            bind:this={loadMoreSentinel}
            data-load-more-sentinel
            class="min-h-12 flex items-center justify-center"
        >
            {#if !supportsAutomaticLoading}
                <button
                    type="button"
                    class="m-2 px-4 py-2 rounded-md border border-darkborderc hover:bg-selected"
                    onclick={revealNextPage}
                    aria-label={language.loadMore}
                >
                    {language.loadMore}
                </button>
            {/if}
        </div>
    {/if}
</div>

<button class="p-4 rounded-full absolute bottom-2 right-2 bg-borderc" onclick={() => {
    addCharacter()
}}>
    <PlusIcon size={24} />
</button>

<style>
    .archived-card {
        filter: grayscale(1);
        opacity: 0.7;
    }
</style>
