<script lang="ts">
    import ChatBody from './ChatBody.svelte'
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
        bind:translated
        bind:translating
        bind:retranslate
        modelShortName=""
    />
</div>
