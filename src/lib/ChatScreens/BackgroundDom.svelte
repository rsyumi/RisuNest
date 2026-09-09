<script lang="ts">
    import { risuChatParser } from 'src/ts/parser/parser.svelte'
    import { type character, type groupChat } from 'src/ts/storage/database.svelte'
    import { DBState } from 'src/ts/stores.svelte'
    import { moduleBackgroundEmbedding, ReloadGUIPointer, selIdState } from 'src/ts/stores.svelte'
    import DeferredMarkdown from 'src/lib/UI/DeferredMarkdown.svelte'
    import LiveDisplayParserBoundary from './LiveDisplayParserBoundary.svelte'

    let backgroundHTML = $derived(DBState.db?.characters?.[selIdState.selId]?.backgroundHTML)
    let currentChar: character | groupChat = $derived(DBState.db?.characters?.[selIdState.selId])
</script>

{#if backgroundHTML || $moduleBackgroundEmbedding}
    {#if selIdState.selId > -1}
        {#key $ReloadGUIPointer}
            <div class="absolute top-0 left-0 w-full h-full">
                <LiveDisplayParserBoundary
                    source={(backgroundHTML || '') + '\n' + ($moduleBackgroundEmbedding || '')}
                    character={currentChar}
                >
                    {#snippet children(signal)}
                        <DeferredMarkdown
                            data={risuChatParser(
                                (backgroundHTML || '') + '\n' + ($moduleBackgroundEmbedding || ''),
                                { chara: currentChar },
                            )}
                            character={currentChar}
                            mode="back"
                            {signal}
                        />
                    {/snippet}
                </LiveDisplayParserBoundary>
            </div>
        {/key}
    {/if}
{/if}
