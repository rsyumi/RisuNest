<script lang="ts">
    import { onMount } from 'svelte'

    let { message, parserAbortSignal, character }: any = $props()
    onMount(() => {
        window.dispatchEvent(
            new CustomEvent('bookmark-chat-mounted', {
                detail: {
                    signal: parserAbortSignal,
                    character,
                    triggerCode:
                        character?.triggerscript?.[0]?.effect?.[0]?.code,
                },
            }),
        )
    })
</script>

<div data-bookmark-chat>{message}</div>
