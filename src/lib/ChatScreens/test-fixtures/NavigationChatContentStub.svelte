<script lang="ts">
    import ChatWindowLayer from '../ChatWindowLayer.svelte'
    let {
        openChatList = $bindable(false),
    }: {
        openChatList?: boolean
    } = $props()
</script>

<button data-old-chat-control onclick={() => (openChatList = true)}
    >Open chat list</button
>

<ChatWindowLayer><button data-window-dialog>Close dialog</button></ChatWindowLayer>
