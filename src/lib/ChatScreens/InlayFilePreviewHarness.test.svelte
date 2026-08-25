<script lang="ts">
    import InlayFilePreview from './InlayFilePreview.svelte'

    let id = $state('first-id')

    export function setId(nextId: string) {
        id = nextId
    }
</script>

<InlayFilePreview {id} />
