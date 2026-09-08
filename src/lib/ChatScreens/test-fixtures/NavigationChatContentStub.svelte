<script lang="ts">
    let {
        openChatList = $bindable(false),
        openModuleList = $bindable(false),
    }: {
        openChatList?: boolean
        openModuleList?: boolean
    } = $props()
</script>

<button data-old-chat-control onclick={() => (openChatList = true)}
    >Open chat list</button
>
