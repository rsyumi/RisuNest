<script lang="ts">
    import ChatWindowLayer from '../../src/lib/ChatScreens/ChatWindowLayer.svelte'

    let { openChatList = $bindable(false), customStyle = '' }: { openChatList?: boolean, customStyle?: string } = $props()
    const query = new URLSearchParams(location.search)
</script>

{#if query.has('home')}
    <!-- Placed like the hub's dialogs on the home screen. -->
    <div data-testid="hub-dialog" style="position: fixed; top: 0; left: 0; width: 100%; height: 100%; z-index: 2000; background: rgba(0, 0, 0, 0.5)"></div>
{:else}
    <!-- Fixed layers as cards and modules write them for the whole window: a background, an effect above everything, a centered status bar and a HUD in the top left corner. -->
    <div data-testid="background" style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; z-index: -10; background: rgb(0, 0, 255)"></div>
    <div data-testid="effect" style="position: fixed; top: 100px; left: 0; width: 100vw; height: 40px; z-index: 9999; background: rgb(0, 255, 0)"></div>
    <div style="position: fixed; top: 0; left: 0; width: 100%; z-index: 1; display: flex; flex-direction: column; align-items: center; pointer-events: none" data-open={openChatList} data-style={customStyle}>
        <div data-testid="status-bar" style="width: 300px; height: 40px; background: rgb(255, 255, 255); pointer-events: auto"></div>
    </div>
    <div data-testid="hud" style="position: fixed; top: 0; left: 0; width: 200px; height: 80px; z-index: 9999; background: rgb(255, 255, 0)"></div>
    {#if query.has('dialog')}
        <!-- A chat dialog that covers the whole window, like the screenshot dialog. -->
        <ChatWindowLayer>
            <div data-testid="window-dialog" style="position: fixed; inset: 0; z-index: 1000; background: rgba(0, 0, 0, 0.5)"></div>
        </ChatWindowLayer>
    {/if}
{/if}
