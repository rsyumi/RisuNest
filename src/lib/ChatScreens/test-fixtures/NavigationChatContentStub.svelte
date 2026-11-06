<script lang="ts">
    let {
        openChatList = $bindable(false),
    }: {
        openChatList?: boolean
    } = $props()
</script>

<button data-old-chat-control onclick={() => (openChatList = true)}
    >Open chat list</button
>
