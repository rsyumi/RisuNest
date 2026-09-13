<script lang="ts">
    import { MobileGUIStack, MobileSideBar, selectedCharID } from "src/ts/stores.svelte";
    import RealmMain from "../UI/Realm/RealmMain.svelte";
    import MobileCharacters from "./MobileCharacters.svelte";
    import ChatScreen from "../ChatScreens/ChatScreen.svelte";
    import CharConfig from "../SideBars/CharConfig.svelte";
    import SelectedConversationEditor from "../SideBars/SelectedConversationEditor.svelte";
    import { WrenchIcon } from "@lucide/svelte";
    import { language } from "src/lang";
    import SideChatList from "../SideBars/SideChatList.svelte";
    import DevTool from "../SideBars/DevTool.svelte";
    import { isLite } from "src/ts/lite";
    
    import { DBState } from 'src/ts/stores.svelte';
    import LoadingIndicator from '../UI/GUI/LoadingIndicator.svelte';
    import { navigationActivity } from '../../ts/ui/navigationActivity';

    let settingsPromise: Promise<typeof import('../Setting/Settings.svelte')> | undefined
    let chatScreenVisible = $derived($MobileSideBar === 0 && $selectedCharID !== -1)

    const loadSettings = () => settingsPromise ??= import('../Setting/Settings.svelte')
</script>

{#if $MobileSideBar > 0 && !$isLite}
<div class="w-full px-2 py-1 text-textcolor2 border-b border-b-darkborderc bg-darkbg flex justify-start items-center gap-2">
    <button class="flex-1 border-r border-r-darkborderc" class:text-textcolor={$MobileSideBar === 1} onclick={() => {
        $MobileSideBar = 1
    }}>
        {language.Chat}
    </button>
    <button class="flex-1 border-r border-r-darkborderc" class:text-textcolor={$MobileSideBar === 2} onclick={() => {
        $MobileSideBar = 2
    }}>
        {language.character}
    </button>
    <button class:text-textcolor={$MobileSideBar === 3} onclick={() => {
        $MobileSideBar = 3
    }}>
        <WrenchIcon size={18} />
    </button>
</div>
{/if}
<div class="w-full flex-1 overflow-y-auto bg-bgcolor relative" aria-busy={$navigationActivity !== null && !chatScreenVisible}>
    <div class="w-full h-full">
    {#if $MobileSideBar > 0}
        <div class="w-full flex flex-col p-2 mt-2 h-full">
            {#if $MobileSideBar === 1}
                <SelectedConversationEditor>
                    <SideChatList bind:chara={DBState.db.characters[$selectedCharID]} />
                </SelectedConversationEditor>
            {:else if $MobileSideBar === 2}
                <SelectedConversationEditor>
                    <CharConfig />
                </SelectedConversationEditor>
            {:else if $MobileSideBar === 3}
                <DevTool />
            {/if}
        </div>
    {:else if $selectedCharID !== -1}
        <ChatScreen />
    {:else if $MobileGUIStack === 0}
        <RealmMain />
    {:else if $MobileGUIStack === 1}
        <MobileCharacters />
    {:else if $MobileGUIStack === 2}
        {#await loadSettings()}
            <div class="w-full h-full flex items-center justify-center text-textcolor">
                <LoadingIndicator label={language.loading} />
            </div>
        {:then module}
            {@const Settings = module.default}
            <Settings />
        {:catch}
            <div class="w-full h-full flex flex-col gap-3 items-center justify-center text-textcolor" role="alert">
                <span>{language.error}</span>
                <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 hover:bg-selected" onclick={() => {settingsPromise = undefined; $MobileGUIStack = 1}}>{language.cancel}</button>
            </div>
        {/await}
    {/if}
    </div>
    {#if $navigationActivity && !chatScreenVisible}
        <div class="pointer-events-none absolute inset-x-0 top-2 z-30 flex justify-center">
            <div class="rounded-md border border-darkborderc bg-darkbg/90 px-3 py-2 text-textcolor shadow-sm">
                <LoadingIndicator label={language.loadingChatData} compact />
            </div>
        </div>
    {/if}
</div>
