<script lang="ts">
    import DefaultChatScreen from './DefaultChatScreen.svelte'
    import Sidebar from '../SideBars/Sidebar.svelte'
    import MobileBody from '../Mobile/MobileBody.svelte'
    import BookmarkList from '../Others/BookmarkList.svelte'
    import ModuleScopeMenu from '../Setting/Pages/Module/ModuleScopeMenu.svelte'
    import { bookmarkListOpen, moduleScopeMenuOpen } from 'src/ts/stores.svelte'

    let { surface }: { surface: 'desktop' | 'mobile' | 'bookmark' } = $props()
    let shown = $state(true)
    export function showMenu(value: boolean) { shown = value }
</script>

{#if surface === 'mobile'}
    <MobileBody />
{:else}
    <div class="flex h-full">
        {#if surface === 'desktop'}
            <aside data-interaction-menu><Sidebar hidden={!shown} /></aside>
        {/if}
        <section data-interaction-chat><DefaultChatScreen /></section>
    </div>
{/if}
{#if surface === 'bookmark' && $bookmarkListOpen}
    <div data-interaction-bookmarks><BookmarkList /></div>
{/if}
<!-- App.svelte hosts the module menu the chat list opens. -->
{#if $moduleScopeMenuOpen}
    <ModuleScopeMenu close={() => moduleScopeMenuOpen.set(false)} />
{/if}
