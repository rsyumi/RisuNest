<script lang="ts">
    import { tick } from 'svelte'
    import AppLockGate from './lib/AppLock/AppLockGate.svelte'
    import { appLock } from './ts/appLock/appLock.svelte'
    import { registerAppLockSurface, setAppLockInputBlocked } from './ts/appLock/input'
    import LazyScreenError from "./lib/UI/LazyScreenError.svelte";
    import ChatBindingLifecycle from './lib/SideBars/ChatBindingLifecycle.svelte'
    import { DynamicGUI, settingsOpen, sideBarStore, sideBarClosing, ShowRealmFrameStore, openPresetList, openPersonaList, MobileGUI, CustomGUISettingMenuStore, loadedStore, alertStore, LoadingStatusState, bookmarkListOpen, popupStore, easyPanelStore, popUpEditorStore, loadoutModalStore, onboardingModalStore, irisStore, customSideBarConfigDialogStore, bootFailure, recoveryStart } from './ts/stores.svelte';
    import Sidebar from './lib/SideBars/Sidebar.svelte';
    import { DBState } from './ts/stores.svelte';
    import ChatScreen from './lib/ChatScreens/ChatScreen.svelte';
    import AlertComp from './lib/Others/AlertComp.svelte';
    import RealmPopUp from './lib/UI/Realm/RealmPopUp.svelte';
    import Onboarding from './lib/Others/Onboarding/Onboarding.svelte';
    import { onboardingHold } from './lib/Others/Onboarding/onboardingGate';
    import BookmarkList from './lib/Others/BookmarkList.svelte';
    import { showRealmInfoStore, importCharacterProcess } from './ts/characterCards';
    import { importPreset, getDatabase, setDatabase } from './ts/storage/database.svelte';
    import { readModule } from './ts/process/modules';
    import { alertNormal } from './ts/alert';
    import { language } from './lang';
    import RealmFrame from './lib/UI/Realm/RealmFrame.svelte';
    import SavePopupIconComp from './lib/Others/SavePopupIcon.svelte';
    import MobileHeader from './lib/Mobile/MobileHeader.svelte';
    import MobileBody from './lib/Mobile/MobileBody.svelte';
    import MobileFooter from './lib/Mobile/MobileFooter.svelte';
    import { checkCharOrder } from './ts/globalApi.svelte';
    import { ArrowUpIcon, GlobeIcon, PlusIcon } from '@lucide/svelte';
    import { hypaV3ModalOpen, hypaV3ProgressStore } from "./ts/stores.svelte";
    import HypaV3Modal from './lib/Others/HypaV3Modal.svelte';
    import HypaV3Progress from './lib/Others/HypaV3Progress.svelte';
    import PopupList from './lib/UI/PopupList.svelte';
    import EasyPanel from './lib/Others/ProTools/EasyPanel.svelte';
    import PopupEditor from './lib/Others/PopupEditor.svelte';
    import TextEditorPopup from './lib/Others/TextEditorPopup.svelte';
    import { textEditorPopup } from './ts/gui/textEditorPopup.svelte';
    import LoadoutModal from './lib/Others/LoadoutModal.svelte';
    import OnboardingModal from './lib/Others/OnboardingModal.svelte';
    import IrisModal from './lib/Others/IrisModal.svelte';
    import Legal from './lib/Others/Legal.svelte';
    import CustomSidebarConfig from './lib/Others/CustomSidebarConfig.svelte';
    import { RISU_APP_INTERNAL_DRAG_TYPE, RISU_SIDEBAR_DRAG_TYPE } from './ts/dragTypes';
    import { keepFocusedInputVisible } from './ts/gui/imeVisibility';
    import { isTauri, isTauriMobile } from './ts/platform';
    import NativeFileJobDialog from './lib/Others/NativeFileJobDialog.svelte';
    import UpdatePopup from './lib/Others/UpdatePopup.svelte';
    import RecoveryShell from './lib/Others/RecoveryShell.svelte';
    import BootFailurePanel from './lib/Others/BootFailurePanel.svelte';
    import { isStartupExcluded, type RecoveryExclusion } from './ts/storage/recoveryMode.svelte';
    import { offerToKeepRecoveryExclusions } from './ts/storage/recoveryExclusionPrompt';
    import { getStartupExclusions } from './ts/storage/deviceSettings';
    import LoadingIndicator from './lib/UI/GUI/LoadingIndicator.svelte';
    import SyncExitDialog from './lib/Others/SyncExitDialog.svelte';
    import PluginStartupNotice from './lib/Others/PluginStartupNotice.svelte';
    import PersistentLocalSaveFailure from './lib/Others/PersistentLocalSaveFailure.svelte';
    import SyncStoppedNotice from './lib/Others/SyncStoppedNotice.svelte';
    import SyncStatusPanel from './lib/Others/SyncStatusPanel.svelte';
    import PersistentWorkingSetRecovery from './lib/Others/PersistentWorkingSetRecovery.svelte';
    import { persistentWorkingSetInputBlocked } from './ts/storage/persistentDataRuntime.svelte';
    import { restoreFocusAfterInputBlock } from './ts/ui/restoreFocusAfterInputBlock';
    import { backNavigationLayer, handleRootBack } from './ts/ui/modalNavigation';

    import {
        serverSyncNavigation,
        serverSyncScreenRequest,
    } from './ts/storage/sync/serverSyncDeepLink'
    import { SettingsMenuIndex } from './ts/stores.svelte'
    import {
        dataHealthNavigation,
        DATA_HEALTH_SECTION_ID,
    } from './ts/storage/dataHealthNavigation'
    import { openRisuNestSettingsTab } from './ts/setting/risuNestSettingsTabs'

    const bootFailureVisible = $derived(!$loadedStore && !$recoveryStart && !!$bootFailure)
    const appLockVisible = $derived(isTauri && !bootFailureVisible && ($loadedStore || !!$recoveryStart) && appLock.gated)

    const appLockFramesHidden = $derived(isTauri && !bootFailureVisible && appLock.gated)

    $effect.pre(() => {
        setAppLockInputBlocked(appLockVisible, appLockFramesHidden)
        return () => setAppLockInputBlocked(false)
    })

    $effect(() => {
        if (!isTauri || !appLock.state.initialized) return
        const update = () => {
            const color = getComputedStyle(document.documentElement).getPropertyValue('--risu-theme-bgcolor').trim()
            if (color) appLock.setBackgroundColor(color)
        }
        update()
        const observer = new MutationObserver(update)
        observer.observe(document.documentElement, { attributes: true, attributeFilter: ['style', 'class'] })
        return () => observer.disconnect()
    })

    $effect(() => {
        if (!isTauri) return
        const failure = bootFailureVisible
        const loading = !$loadedStore && !$recoveryStart && !$bootFailure
        const epoch = appLock.state.epoch
        const presentation = appLock.state.presentationRevision
        const initialized = appLock.state.initialized
        if (!failure && !loading && (!initialized || appLock.gated)) return
        let cancelled = false
        void tick().then(() => requestAnimationFrame(() => requestAnimationFrame(() => {
            if (cancelled || epoch !== appLock.state.epoch || presentation !== appLock.state.presentationRevision) return
            if (failure) void appLock.renderBootFailure()
            else if (loading) void appLock.renderLoading()
            else void appLock.rendered(language.risuNest.appLock.promptUnlock)
        })))
        return () => { cancelled = true }
    })

    $effect(() => restoreFocusAfterInputBlock(persistentWorkingSetInputBlocked))

    $effect(() => {
        window.addEventListener('risunest-root-back', handleRootBack)
        return () => window.removeEventListener('risunest-root-back', handleRootBack)
    })

    $effect(() => {
        if (!isTauri || !$loadedStore) return
        return serverSyncNavigation.subscribe((request) => {
            serverSyncScreenRequest.set(request)
            if (DBState.db.didFirstSetup && !$onboardingHold) {
                openRisuNestSettingsTab('sync')
                SettingsMenuIndex.set(17)
                settingsOpen.set(true)
            }
        })
    })

    $effect(() => {
        if (!isTauri) return
        return dataHealthNavigation.subscribe(() => {
            openRisuNestSettingsTab('storage')
            SettingsMenuIndex.set(17)
            settingsOpen.set(true)
            requestAnimationFrame(() => {
                document
                    .getElementById(DATA_HEALTH_SECTION_ID)
                    ?.scrollIntoView({ block: 'start' })
            })
        })
    })

    let recoveryExcluded: RecoveryExclusion[] = $state([])
    // A start that finished is the proof the exclusions helped, so the offer to keep them comes
    // only then, and only the reader's answer writes anything.
    $effect(() => {
        if (!$loadedStore || recoveryExcluded.length === 0) return
        const excluded = recoveryExcluded
        recoveryExcluded = []
        void offerToKeepRecoveryExclusions(excluded)
    })

    let startupElapsedSeconds = $state(0)
    // `language` is a plain module binding. A start switches it to the library's language right before
    // the next status text, so the label is read again with each one.
    const startupLabel = $derived.by(() => {
        void LoadingStatusState.text
        return language.loading
    })
    $effect(() => {
        const startedAt = LoadingStatusState.startedAt
        if ($loadedStore || $bootFailure || startedAt === null) return
        const updateElapsed = () => {
            startupElapsedSeconds = Math.max(0, Math.floor((performance.now() - startedAt) / 1000))
        }
        updateElapsed()
        const timer = setInterval(updateElapsed, 1000)
        return () => clearInterval(timer)
    })

    $effect(() => {
        if (isTauri && $loadedStore) {
            // The update check runs after the start finishes, so a start that left it off keeps it off.
            if (!isStartupExcluded('autoUpdate', getStartupExclusions()))
                void import('./ts/update/controller').then(({ startAppUpdateChecks }) => startAppUpdateChecks())
        }
    })

    let settingsPromise: Promise<typeof import('./lib/Setting/Settings.svelte')> | undefined
    let gridCharsPromise: Promise<typeof import('./lib/Others/GridCatalog.svelte')> | undefined
    let botpresetPromise: Promise<typeof import('./lib/Setting/botpreset.svelte')> | undefined
    let listedPersonaPromise: Promise<typeof import('./lib/Setting/listedPersona.svelte')> | undefined
    let customGUISettingMenuPromise: Promise<typeof import('./lib/Setting/Pages/CustomGUISettingMenu.svelte')> | undefined

    let lazyRetry = $state(0)

    const loadSettings = (_retry: number) =>
        (settingsPromise ??= import('./lib/Setting/Settings.svelte'))
    const loadGridChars = (_retry: number) =>
        (gridCharsPromise ??= import('./lib/Others/GridCatalog.svelte'))
    const loadBotpreset = (_retry: number) =>
        (botpresetPromise ??= import('./lib/Setting/botpreset.svelte'))
    const loadListedPersona = (_retry: number) =>
        (listedPersonaPromise ??= import('./lib/Setting/listedPersona.svelte'))
    const loadCustomGUISettingMenu = (_retry: number) =>
        (customGUISettingMenuPromise ??=
            import('./lib/Setting/Pages/CustomGUISettingMenu.svelte'))



    let didFirstSetup: boolean  = $derived(DBState.db?.didFirstSetup)
    let gridOpen = $state(false)
    let aprilFools = $state(new Date().getMonth() === 3 && new Date().getDate() === 1)
    let aprilFoolsPage = $state(0)

    const getMainDropEffect = (e:DragEvent): DataTransfer['dropEffect'] => {
        const types = Array.from(e.dataTransfer?.types ?? [])
        if(types.includes(RISU_SIDEBAR_DRAG_TYPE)){
            return 'none'
        }
        if(types.includes(RISU_APP_INTERNAL_DRAG_TYPE)){
            return 'none'
        }
        return types.includes('Files') ? 'copy' : 'none'
    }

    const markAppInternalDrag = (e:DragEvent) => {
        e.dataTransfer?.setData(RISU_APP_INTERNAL_DRAG_TYPE, 'true')
    }

</script>

<ChatBindingLifecycle />

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<main inert={$persistentWorkingSetInputBlocked || appLockVisible} class="flex bg-bg w-full h-full max-w-100vw text-textcolor" use:keepFocusedInputVisible={isTauriMobile} ondragover={(e) => {
    const dropEffect = getMainDropEffect(e)
    e.preventDefault()
    e.dataTransfer.dropEffect = dropEffect
}} ondragstart={markAppInternalDrag} ondrop={async (e) => {
    const types = Array.from(e.dataTransfer.types ?? [])
    if (types.includes(RISU_APP_INTERNAL_DRAG_TYPE) || types.includes(RISU_SIDEBAR_DRAG_TYPE)) {
        e.preventDefault()
        return
    }
    const file = e.dataTransfer.files[0]
    if (!file) {
        e.preventDefault()
        return
    }
    e.preventDefault()
    const name = file.name.toLowerCase()

    if (name.endsWith('.risup')) {
        const data = new Uint8Array(await file.arrayBuffer())
        await importPreset({ name: file.name, data })
        alertNormal(language.successImport)
    } else if (name.endsWith('.risum')) {
        if (isTauri) {
            const { importNativeContentFile } = await import('./ts/storage/nativeContentFile')
            await importNativeContentFile(file, 'module')
            return
        }
        const data = new Uint8Array(await file.arrayBuffer())
        const module = await readModule(Buffer.from(data))
        DBState.db.modules.push(module)
        alertNormal(language.successImport)
    } else {
        await importCharacterProcess({
            name: file.name,
            data: file
        })
        checkCharOrder()
    }
}}>
    {#if !import.meta.env.VITE_RISU_LEGAL_CONFIGURED}
        <Legal />
    {:else if aprilFools}

        <div class="bg-[#212121] w-full h-screen min-h-screen text-black flex relative">
            <div class="w-full max-w-3xl mx-auto py-8 px-4 flex justify-center items-center">
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <div class="flex flex-col w-full items-center text-[#bbbbbb]">
                    {#if aprilFoolsPage === 0}
                        <h1 class="text-3xl text-white font-bold mb-6">What can I help you?</h1>
                        <div class="resize-none relative w-full bg-[#303030] rounded-3xl h-[110px] mb-6 text-[#bbbbbb]" placeholder="Ask me" onkeydown={(e) => {
                            if(e.key === 'Enter'){
                                aprilFoolsPage = 1
                            }
                        }}>
                            <textarea class="absolute top-0 left-0 w-full placeholder-[#bbbbbb] rounded-3xl h-full p-4 bg-transparent resize-none" placeholder="Ask me"></textarea>
                            <div class="absolute bottom-2 left-4 flex gap-1.5">
                                <button class="p-2 rounded-full border border-[#bbbbbb30]">
                                    <PlusIcon size={18} color="#bbbbbb" />
                                </button>
                                <button class="p-2 rounded-full border border-[#bbbbbb30]">
                                    <GlobeIcon size={18} color="#bbbbbb" />
                                </button>

                            </div>
                            <div class="absolute bottom-2 right-4 flex">
                                <button class="p-2 rounded-full bg-[#bbbbbb]">
                                    <ArrowUpIcon size={18} color="#00000080" />
                                </button>
                            </div>
                        </div>
                        <!-- svelte-ignore a11y_click_events_have_key_events -->
                        <div class="flex gap-1.5" onclick={() => {
                            aprilFoolsPage = 1
                        }}>
                            <button class="rounded-full border border-[#bbbbbb15] px-4 py-2">
                                <span class="text-[#bbbbbb]">🔍</span>
                                Search
                            </button>
                            <button class="rounded-full border border-[#bbbbbb15] px-4 py-2">
                                <span class="text-[#bbbbbb]">🎮</span>
                                Games
                            </button>
                            <button class="rounded-full border border-[#bbbbbb15] px-4 py-2">
                                <span class="text-[#bbbbbb]">🎨</span>
                                Roleplay
                            </button>
                            <button class="rounded-full border border-[#bbbbbb15] px-4 py-2">
                                More
                            </button>
                        </div>
                    {:else}
                    <h1 class="text-3xl text-white font-bold mb-6">
                        We do not have search results.
                    </h1>
                    <p class="text-[#bbbbbb] mb-6">
                        <!-- svelte-ignore a11y_missing_attribute -->
                        <!-- svelte-ignore a11y_click_events_have_key_events -->
                        <a class="text-blue-500 cursor-pointer" onclick={() => {
                            aprilFoolsPage = 0
                            aprilFools = false
                        }}>
                            Go to RisuNest
                        </a>
                    </p>

                    {/if}
                </div>
            </div>
            <span class="absolute top-4 left-4 font-bold text-[#bbbbbb] text-md md:text-lg">RisyGTP 9+ Mytho Ultra Free</span>
        </div>
    {:else if !$loadedStore}
        {#if $recoveryStart}
            <RecoveryShell
                onStart={(excluded) => {
                    const start = $recoveryStart
                    recoveryStart.set(null)
                    recoveryExcluded = [...excluded]
                    start?.()
                }}
            />
        {:else if $bootFailure}
            <BootFailurePanel failure={$bootFailure} />
        {:else}
            <div
                class="w-full h-full flex justify-center items-center text-textcolor text-xl bg-darkbg"
            >
                <LoadingIndicator
                    label={startupLabel}
                    detail={LoadingStatusState.text}
                    elapsedText={language.risuNest.startup.elapsed(startupElapsedSeconds)}
                />
            </div>
        {/if}
    {:else if $CustomGUISettingMenuStore}
        {#await loadCustomGUISettingMenu(lazyRetry)}
            <div
                class="w-full h-full flex items-center justify-center text-textcolor"
            >
                <LoadingIndicator label={language.loading} />
            </div>
        {:then module}
            {@const CustomGUISettingMenu = module.default}
            <CustomGUISettingMenu />
        {:catch}
            <LazyScreenError message={language.risuNest.lazy.screen} onRetry={() => { customGUISettingMenuPromise = undefined; lazyRetry++ }} backLabel={language.close} onBack={() => { customGUISettingMenuPromise = undefined; $CustomGUISettingMenuStore = false }} />
        {/await}
    {:else if !didFirstSetup || $onboardingHold}
        <Onboarding />
    {:else if $settingsOpen}
        {#await loadSettings(lazyRetry)}
            <div
                class="w-full h-full flex items-center justify-center text-textcolor"
            >
                <LoadingIndicator label={language.loading} />
            </div>
        {:then module}
            {@const Settings = module.default}
            {#if $MobileGUI}
                <div class="w-full h-full flex flex-col">
                    <MobileHeader />
                    <div class="flex-1 min-h-0"><Settings /></div>
                </div>
            {:else}
                <Settings />
            {/if}
        {:catch}
            <LazyScreenError message={language.risuNest.lazy.settings} onRetry={() => { settingsPromise = undefined; lazyRetry++ }} backLabel={language.close} onBack={() => { settingsPromise = undefined; $settingsOpen = false }} />
        {/await}
    {:else if $MobileGUI}
        <div class="w-full h-full flex flex-col">
            <MobileHeader />
            <MobileBody />
            <MobileFooter />
        </div>
    {:else}
        {#if gridOpen}
            {#await loadGridChars(lazyRetry)}
                <div
                    class="w-full h-full flex items-center justify-center text-textcolor"
                >
                    <LoadingIndicator label={language.loading} />
                </div>
            {:then module}
                {@const GridChars = module.default}
                <GridChars
                    endGrid={() => {
                        gridOpen = false
                    }}
                />
            {:catch}
            <LazyScreenError message={language.risuNest.lazy.screen} onRetry={() => { gridCharsPromise = undefined; lazyRetry++ }} backLabel={language.close} onBack={() => { gridCharsPromise = undefined; gridOpen = false }} />
            {/await}
        {:else}
            {#if (!$DynamicGUI)}
                <Sidebar openGrid={() => {gridOpen = true}} hidden={!$sideBarStore} />
            {:else}
                <!-- Back closes the open sidebar; Escape stays with the editors inside it. -->
                <div class="top-0 w-full h-full left-0 z-30 flex flex-row items-center" class:fixed={$sideBarStore} class:hidden={!$sideBarStore} use:backNavigationLayer={{ enabled: $sideBarStore, close: () => sideBarClosing.set(true), leaveEscape: () => true }}>
                    <!-- svelte-ignore a11y_click_events_have_key_events -->
                    <Sidebar openGrid={() => {gridOpen = true}}  hidden={false} />


                </div>
            {/if}
            <ChatScreen />
        {/if}
    {/if}
    <!-- Before the alerts so a refused save or a removed-chat notice shows above the editor. -->
    {#if textEditorPopup.request}
        {#key textEditorPopup.request}
            <TextEditorPopup request={textEditorPopup.request} />
        {/key}
    {/if}
    <!-- Also before the alerts, so a blocked or failed save shows above it. -->
    {#if onboardingModalStore.open}
        <OnboardingModal />
    {/if}
    {#if $alertStore.type !== 'none'}
        <AlertComp />
    {:else if $alertStore.dialogPending}
        <!-- Keeps the backdrop between queued dialogs and takes clicks meant for the closed one. -->
        <div class="absolute w-full h-full z-modal bg-black/50"></div>
    {/if}
    {#if $showRealmInfoStore}
        <RealmPopUp bind:openedData={$showRealmInfoStore} />
    {/if}
    {#if $ShowRealmFrameStore}
        <RealmFrame />
    {/if}
    {#if $openPresetList}
        {#await loadBotpreset(lazyRetry)}
            <div
                class="absolute inset-0 z-40 flex items-center justify-center bg-darkbg text-textcolor"
            >
                <LoadingIndicator label={language.loading} />
            </div>
        {:then module}
            {@const Botpreset = module.default}
            <Botpreset
                close={() => {
                    $openPresetList = false
                }}
            />
        {:catch}
            <LazyScreenError overlay message={language.risuNest.lazy.screen} onRetry={() => { botpresetPromise = undefined; lazyRetry++ }} backLabel={language.close} onBack={() => { botpresetPromise = undefined; $openPresetList = false }} />
        {/await}
    {/if}
    {#if $openPersonaList}
        {#await loadListedPersona(lazyRetry)}
            <div
                class="absolute inset-0 z-40 flex items-center justify-center bg-darkbg text-textcolor"
            >
                <LoadingIndicator label={language.loading} />
            </div>
        {:then module}
            {@const ListedPersona = module.default}
            <ListedPersona
                close={() => {
                    $openPersonaList = false
                }}
            />
        {:catch}
            <LazyScreenError overlay message={language.risuNest.lazy.screen} onRetry={() => { listedPersonaPromise = undefined; lazyRetry++ }} backLabel={language.close} onBack={() => { listedPersonaPromise = undefined; $openPersonaList = false }} />
        {/await}
    {/if}
    {#if $bookmarkListOpen}
        <BookmarkList />
    {/if}
    {#if $hypaV3ModalOpen}
        <HypaV3Modal />
    {/if}
    <SavePopupIconComp />
    {#if $hypaV3ProgressStore.open}
        <HypaV3Progress />
    {/if}
    {#if popupStore.children}
        <PopupList />
    {/if}
    {#if easyPanelStore.open}
        <EasyPanel />
    {/if}
    {#if popUpEditorStore.open}
        <PopupEditor />
    {/if}
    {#if loadoutModalStore.open}
        <LoadoutModal />
    {/if}
    {#if irisStore.open}
        <IrisModal />
    {/if}
    {#if customSideBarConfigDialogStore.open}
        <CustomSidebarConfig />
    {/if}
    <NativeFileJobDialog />
    <UpdatePopup />
</main>
<SyncExitDialog aboveAppLock={appLockVisible} />
<div class="contents" inert={appLockVisible}><PersistentWorkingSetRecovery /></div>
<!-- Open settings place their close button at the top right; the notices start below it. The padding keeps card shadows inside the scroll box. -->
<div inert={appLockVisible} class="pointer-events-none fixed right-0 z-[1000] flex max-h-[calc(100dvh-4rem)] flex-col gap-2 overflow-y-auto px-2 pt-2 pb-4 sm:left-auto sm:w-full sm:max-w-[464px] {$MobileGUI ? 'left-0 top-[64px]' : $settingsOpen ? 'left-0' : 'left-12 top-0'} [&>*]:pointer-events-auto" style:top={$settingsOpen && !$MobileGUI ? `${DBState.db.settingsCloseButtonSize + 8}px` : null}>
    <PersistentLocalSaveFailure />
    <!-- Onboarding shows the sync and plugin state itself. -->
    {#if didFirstSetup && !$onboardingHold}
        <PluginStartupNotice />
        <SyncStoppedNotice />
    {/if}
</div>
<div class="contents" inert={appLockVisible}><SyncStatusPanel /></div>
{#if appLockVisible}
    <AppLockGate onSurface={registerAppLockSurface} />
{/if}
