<script lang="ts">
    import { MenuIcon } from "@lucide/svelte";
    import { popupStore } from "src/ts/stores.svelte";
    import { sleep } from "src/ts/util";

    const {
        children
    }:{
        children: import("svelte").Snippet
    } = $props();
    
    let buttonId = Math.random()
</script>

<button onclick={async (e:MouseEvent) => {
    // A menu closed by a tap elsewhere keeps this button's id, so the id alone does not mean it is open.
    // Read before the wait: this tap also reaches the menu's own handler, which closes it.
    const open = popupStore.children !== null && popupStore.openId === buttonId
    await sleep(0)
    if(open){
        popupStore.children = null
        popupStore.openId = 0
        return
    }
    popupStore.mouseX = e.clientX
    popupStore.mouseY = e.clientY
    popupStore.children = children
    popupStore.openId = buttonId
}} class="hover:text-blue-500 transition-colors button-icon-menu">
    <MenuIcon size={20} />
</button>