<script lang="ts">
    import ChatBody from './ChatBody.svelte'
    import type { ChatDisplayRefresh } from 'src/ts/chatDisplayRefresh'
    let {
        message,
        character,
        idx = 0,
        name,
        role,
        parserProjection,
        parserAbortSignal,
    }: any = $props()
    let translated = $state(false)
    let translating = $state(false)
    let retranslate = $state(false)
    let refreshRevision = $state(0)
    export function refreshMessageDisplay(state: ChatDisplayRefresh) {
        message = state.message
        parserProjection = state.parserProjection
        parserAbortSignal = state.parserAbortSignal
        refreshRevision += 1
    }
    export function updateViewportBinding(state: any) {
        parserProjection = state.parserProjection
    }

</script>

<div data-lua-body data-index={idx}>
    <ChatBody
        msgDisplay={message}
        {character}
        {idx}
        {name}
        {role}
        {parserProjection}
        {parserAbortSignal}
        reloadRevision={String(refreshRevision)}
        bind:translated
        bind:translating
        bind:retranslate
        modelShortName=""
    />
</div>
