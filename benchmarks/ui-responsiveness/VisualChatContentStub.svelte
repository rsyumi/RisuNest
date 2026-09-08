<script lang="ts">
    export let openChatList = false
    export let openModuleList = false
    export let customStyle = ''

    function recordActivation() {
        window.__risuUiVisualUnderlyingActivations += 1
    }
</script>

<section class="synthetic-chat-content" data-synthetic-chat-content>
    <p>Local synthetic chat surface</p>
    <button data-synthetic-underlying-control onclick={recordActivation}
        >Synthetic action</button
    >
    <button
        onclick={() => {
            openChatList = true
        }}>Synthetic chat list</button
    >
    <button
        onclick={() => {
            openModuleList = true
        }}>Synthetic module list</button
    >
</section>

<style>
    .synthetic-chat-content {
        box-sizing: border-box;
        height: 100%;
        padding: 2rem;
        color: var(--risu-theme-textcolor);
        background: linear-gradient(
            135deg,
            var(--risu-theme-darkbg),
            var(--risu-theme-bgcolor)
        );
    }

    button {
        margin: 0.5rem;
        border: 1px solid var(--risu-theme-borderc);
        border-radius: 0.5rem;
        padding: 0.5rem 0.75rem;
    }
</style>
