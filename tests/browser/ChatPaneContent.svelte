<script lang="ts">
    let { openChatList = $bindable(false), customStyle = '' }: { openChatList?: boolean, customStyle?: string } = $props()
</script>

<!-- Fixed layers as cards and modules write them for the whole window: a background, an effect above everything, and a centered status bar. -->
<div data-testid="background" style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; z-index: -10; background: rgb(0, 0, 255)"></div>
<div data-testid="effect" style="position: fixed; top: 100px; left: 0; width: 100vw; height: 40px; z-index: 9999; background: rgb(0, 255, 0)"></div>
<div style="position: fixed; top: 0; left: 0; width: 100%; z-index: 1; display: flex; flex-direction: column; align-items: center; pointer-events: none" data-open={openChatList} data-style={customStyle}>
    <div data-testid="status-bar" style="width: 300px; height: 40px; background: rgb(255, 255, 255); pointer-events: auto"></div>
</div>
