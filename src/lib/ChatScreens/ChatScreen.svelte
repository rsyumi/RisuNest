<script lang="ts">
    import { backNavigationLayer } from 'src/ts/ui/modalNavigation';
    import { getCustomBackground, getEmotion } from "../../ts/util";
    
    import { DBState } from 'src/ts/stores.svelte';
    import { CharEmotion, selectedCharID } from "../../ts/stores.svelte";
    import ResizeBox from './ResizeBox.svelte'
    import DefaultChatScreen from "./DefaultChatScreen.svelte";
    import defaultWallpaper from '../../etc/bg.jpg'
    import ChatList from "../Others/ChatList.svelte";
    import TransitionImage from "./TransitionImage.svelte";
    import BackgroundDom from "./BackgroundDom.svelte";
    import SideBarArrow from "../UI/GUI/SideBarArrow.svelte";
    import SelectedConversationEditor from "../SideBars/SelectedConversationEditor.svelte";
    import LoadingIndicator from '../UI/GUI/LoadingIndicator.svelte';
    import { language } from '../../lang';
    import { navigationActivity } from '../../ts/ui/navigationActivity';
    import { derived } from 'svelte/store';
    import { onDestroy } from 'svelte';
    import { restoreFocusAfterInputBlock } from '../../ts/ui/restoreFocusAfterInputBlock';
    import { provideChatWindowLayer } from './ChatWindowLayer.svelte';
    const navigationBlocked = derived(navigationActivity, activity => activity !== null)
    onDestroy(restoreFocusAfterInputBlock(navigationBlocked))
    let openChatList = $state(false)
    let windowLayer = $state<HTMLElement>()
    provideChatWindowLayer(() => windowLayer)

    const wallPaper = `background: url(${defaultWallpaper})`
    const externalStyles = 
            ("background: " + (DBState.db.textScreenColor ? (DBState.db.textScreenColor + '80') : "rgba(0,0,0,0.8)") + ';\n')
        +   (DBState.db.textBorder ? "text-shadow: -1px -1px 0 #000, 1px -1px 0 #000, -1px 1px 0 #000, 1px 1px 0 #000;" : '')
        +   (DBState.db.textScreenRounded ? "border-radius: 2rem; padding: 1rem;" : '')
        +   (DBState.db.textScreenBorder ? `border: 0.3rem solid ${DBState.db.textScreenBorder};` : '')
    let bgImg= $state('')
    let lastBg = $state('')
    $effect.pre(() => {
        (async () =>{
            if(DBState.db.customBackground !== lastBg){
                lastBg = DBState.db.customBackground
                bgImg = await getCustomBackground(DBState.db.customBackground)
            }
        })()
    });
</script>

<div class="relative min-w-0 grow h-full" aria-busy={$navigationBlocked}>
    <!-- Paint containment lays out fixed layers from card and module HTML in the chat pane and clips them to it, so they never cover the sidebar. -->
    <div class="relative z-0 min-w-0 h-full contain-paint" inert={$navigationBlocked}>
    {#if DBState.db.theme === 'waifu'}
        <div class="grow h-full flex justify-center relative" style="{bgImg.length < 4 ? wallPaper : bgImg}">
            <BackgroundDom />
            {#if $selectedCharID >= 0}
                {#if DBState.db.characters[$selectedCharID].viewScreen !== 'none'}
                    <div class="h-full mr-10 flex justify-end halfw" style:width="{42 * (DBState.db.waifuWidth2 / 100)}rem">
                        <TransitionImage classType="waifu" src={getEmotion(DBState.db, $CharEmotion, 'plain')}/>
                    </div>
                {/if}
            {/if}
            <div class="h-full w-2xl" style:width="{42 * (DBState.db.waifuWidth / 100)}rem" class:halfwp={$selectedCharID >= 0 && DBState.db.characters[$selectedCharID].viewScreen !== 'none'}>
                <DefaultChatScreen customStyle={`${externalStyles}backdrop-filter: blur(4px);`} bind:openChatList/>
            </div>
        </div>
    {:else if DBState.db.theme === 'waifuMobile'}
        <div class="grow h-full relative" style={bgImg.length < 4 ? wallPaper : bgImg}>
            <BackgroundDom />
            <div class="w-full absolute z-10 bottom-0 left-0"
                class:per33={$selectedCharID >= 0 && DBState.db.characters[$selectedCharID].viewScreen !== 'none'}
                class:h-full={!($selectedCharID >= 0 && DBState.db.characters[$selectedCharID].viewScreen !== 'none')}
            >
                <DefaultChatScreen customStyle={`${externalStyles}backdrop-filter: blur(4px);`} bind:openChatList/>
            </div>
            {#if $selectedCharID >= 0}
                {#if DBState.db.characters[$selectedCharID].viewScreen !== 'none'}
                    <div class="h-full w-full absolute bottom-0 left-0 max-w-full">
                        <TransitionImage classType="mobile" src={getEmotion(DBState.db, $CharEmotion, 'plain')}/>
                    </div>
                {/if}
            {/if}
        </div>
    {:else}
        <div class="grow h-full min-w-0 relative justify-center flex">
            <BackgroundDom />
            <div style={bgImg} class="h-full w-full" class:max-w-6xl={DBState.db.classicMaxWidth}>
                {#if $selectedCharID >= 0}
                    {#if DBState.db.characters[$selectedCharID].viewScreen !== 'none' && (!DBState.db.characters[$selectedCharID].inlayViewScreen)}
                        <ResizeBox />
                    {/if}
                {/if}
                <DefaultChatScreen customStyle={bgImg.length > 2 ? `${externalStyles}`: ''} bind:openChatList/>
            </div>
        </div>
    {/if}
    </div>
    <!-- Above the chat and its HTML, below the loading overlay and the conversation list. -->
    <div class="absolute top-0 left-0 z-10" inert={$navigationBlocked}>
        <SideBarArrow />
    </div>
    <!-- Dialogs in the chat screen that cover the whole window. They sit on the app's dialog layers rather than under the chat screen's overlays, so they are hidden while another conversation loads. -->
    <div class="absolute top-0 left-0" hidden={$navigationBlocked} bind:this={windowLayer}></div>
    {#if $navigationActivity}
        <div class="absolute inset-0 z-30 flex items-center justify-center bg-darkbg/70">
            <LoadingIndicator label={language.loadingChatData} />
        </div>
    {/if}
    {#if openChatList}
        <div class="absolute inset-0 z-40" use:backNavigationLayer={{ close: () => { openChatList = false } }}>
            <SelectedConversationEditor
                close={() => {
                    openChatList = false;
                }}
            >
                <ChatList
                    close={() => {
                        openChatList = false;
                    }}
                />
            </SelectedConversationEditor>
        </div>
    {/if}
</div>

<style>
    .halfw{
        max-width: calc(50% - 5rem);
    }
    .halfwp{
        max-width: calc(50% - 5rem);
    }
    .per33{
        height: 33.333333%;
    }
</style>
