<script lang="ts">
    import DeferredMarkdown from './DeferredMarkdown.svelte'

    let data = $state('first')
    let controller = $state(new AbortController())
    export function refresh(value = data) {
        controller.abort()
        controller = new AbortController()
        data = value
    }

</script>

<DeferredMarkdown {data} signal={controller.signal} preservePendingContent />
