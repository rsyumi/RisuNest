<script lang="ts">
    import ChatScreen from '../../src/lib/ChatScreens/ChatScreen.svelte'
</script>

<main id="ui-visual-qa-root">
    <section class="pane" data-synthetic-pane>
        <ChatScreen />
    </section>
</main>

<style>
    :global(html),
    :global(body),
    :global(#app),
    #ui-visual-qa-root {
        height: 100%;
        margin: 0;
    }

    :global(body) {
        overflow: hidden;
        font-family: system-ui, sans-serif;
    }

    #ui-visual-qa-root {
        box-sizing: border-box;
        display: grid;
        place-items: center;
        padding: 40px 16px;
        background: #161820;
    }

    .pane {
        width: min(720px, 100%);
        height: min(600px, 100%);
        overflow: hidden;
        color: var(--risu-theme-textcolor);
        border: 2px solid var(--risu-theme-borderc);
        border-radius: 12px;
        box-shadow: 0 20px 60px rgb(0 0 0 / 45%);
    }
</style>
