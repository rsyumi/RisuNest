<script lang="ts">
    import ChatBindingLifecycle from './lib/SideBars/ChatBindingLifecycle.svelte'
    import { DynamicGUI, settingsOpen, sideBarStore, ShowRealmFrameStore, openPresetList, openPersonaList, MobileGUI, CustomGUISettingMenuStore, loadedStore, alertStore, LoadingStatusState, bookmarkListOpen, popupStore, easyPanelStore, popUpEditorStore, loadoutModalStore, irisStore, customSideBarConfigDialogStore, bootFailure, recoveryStart, type BootFailure } from './ts/stores.svelte';
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
    import { alertConfirm, alertNormal, alertToast } from './ts/alert';
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
    import sendSound from './etc/send.mp3'
    import PopupEditor from './lib/Others/PopupEditor.svelte';
    import LoadoutModal from './lib/Others/LoadoutModal.svelte';
    import IrisModal from './lib/Others/IrisModal.svelte';
    import Legal from './lib/Others/Legal.svelte';
    import CustomSidebarConfig from './lib/Others/CustomSidebarConfig.svelte';
    import { RISU_APP_INTERNAL_DRAG_TYPE, RISU_SIDEBAR_DRAG_TYPE } from './ts/dragTypes';
    import { keepFocusedInputVisible } from './ts/gui/imeVisibility';
    import { isTauri, isTauriMobile } from './ts/platform';
    import NativeFileJobDialog from './lib/Others/NativeFileJobDialog.svelte';
    import UpdatePopup from './lib/Others/UpdatePopup.svelte';
    import RecoveryShell from './lib/Others/RecoveryShell.svelte';
    import {
        confirmRecoveryExclusions,
        isStartupExcluded,
        RECOVERY_EXCLUSIONS,
        type RecoveryExclusion,
    } from './ts/storage/recoveryMode.svelte';
    import { getDeviceSettings, updateDeviceSettings } from './ts/storage/deviceSettings';
    import LoadingIndicator from './lib/UI/GUI/LoadingIndicator.svelte';
    import SyncExitDialog from './lib/Others/SyncExitDialog.svelte';
    import PersistentWorkingSetRecovery from './lib/Others/PersistentWorkingSetRecovery.svelte';
    import { persistentWorkingSetInputBlocked } from './ts/storage/persistentDataRuntime.svelte';
    import { exportOriginalData } from './ts/storage/rawRecoveryExport';

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
    let startupExportMessage = $state('')
    const exportStartupOriginalData = async () => {
        startupExportMessage = ''
        try {
            const result = await exportOriginalData()
            if (!result) return
            startupExportMessage = result.warningCodes.includes('source-problems')
                ? language.risuNest.recovery.exportPartial
                : language.risuNest.recovery.exportComplete
        } catch (error) {
            startupExportMessage = error instanceof DOMException && error.name === 'AbortError'
                ? language.risuNest.recovery.exportCancelled
                : language.risuNest.recovery.exportFailed
        }
    }
    const exclusionName = (exclusion: RecoveryExclusion): string =>
        ({
            plugins: language.risuNest.recovery.excludePlugins,
            modules: language.risuNest.recovery.excludeModules,
            regex: language.risuNest.recovery.excludeRegex,
            theme: language.risuNest.recovery.excludeTheme,
            sync: language.risuNest.recovery.excludeSync,
            autoUpdate: language.risuNest.recovery.excludeAutoUpdate,
            account: language.risuNest.recovery.excludeAccount,
        })[exclusion]
    // A start that finished is the proof the exclusions helped, so the offer to keep them comes
    // only then, and only the reader's answer writes anything.
    $effect(() => {
        if (!$loadedStore || recoveryExcluded.length === 0) return
        const excluded = recoveryExcluded
        recoveryExcluded = []
        void confirmRecoveryExclusions(
            excluded,
            alertConfirm,
            (exclusions) => {
                const kept = new Set([
                    ...getDeviceSettings().startupExclusions,
                    ...exclusions,
                ])
                updateDeviceSettings({
                    startupExclusions: RECOVERY_EXCLUSIONS.filter((item) => kept.has(item)),
                })
            },
            exclusionName,
            language.risuNest.recovery.keepBody,
        )
    })

    let startupElapsedSeconds = $state(0)
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
            // Start only after storage, asset authority and the working set are ready.
            void import('./ts/storage/sync/serverSyncProduction').then(({ startServerSync }) => startServerSync())
            // The update check runs after the start finishes, so a start that left it off keeps it off.
            if (!isStartupExcluded('autoUpdate', getDeviceSettings().startupExclusions))
                void import('./ts/update/controller').then(({ startAppUpdateChecks }) => startAppUpdateChecks())
        }
    })

    let settingsPromise:
        Promise<typeof import('./lib/Setting/Settings.svelte')> | undefined
    let gridCharsPromise:
        Promise<typeof import('./lib/Others/GridCatalog.svelte')> | undefined
    let botpresetPromise:
        Promise<typeof import('./lib/Setting/botpreset.svelte')> | undefined
    let listedPersonaPromise:
        Promise<typeof import('./lib/Setting/listedPersona.svelte')> | undefined
    let customGUISettingMenuPromise:
        | Promise<
              typeof import('./lib/Setting/Pages/CustomGUISettingMenu.svelte')
          >
        | undefined

    const loadSettings = () =>
        (settingsPromise ??= import('./lib/Setting/Settings.svelte'))
    const loadGridChars = () =>
        (gridCharsPromise ??= import('./lib/Others/GridCatalog.svelte'))
    const loadBotpreset = () =>
        (botpresetPromise ??= import('./lib/Setting/botpreset.svelte'))
    const loadListedPersona = () =>
        (listedPersonaPromise ??= import('./lib/Setting/listedPersona.svelte'))
    const loadCustomGUISettingMenu = () =>
        (customGUISettingMenuPromise ??=
            import('./lib/Setting/Pages/CustomGUISettingMenu.svelte'))


  
    let didFirstSetup: boolean  = $derived(DBState.db?.didFirstSetup)
    let gridOpen = $state(false)
    let aprilFools = $state(new Date().getMonth() === 3 && new Date().getDate() === 1)
    let aprilFoolsPage = $state(0)
    let keepingSessionAlive = $state(false)

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

    const bootFailureExplanation = (failure: BootFailure) => {
        switch (failure.kind) {
            case 'schema-unsupported': return language.risuNest.boot.schemaUnsupported
            case 'store-open': return language.risuNest.boot.storeOpen
            default: return language.risuNest.boot.unknown
        }
    }

    const bootFailureDetails = (failure: BootFailure) => [
        language.risuNest.boot.title,
        failure.message,
        failure.stage ? `${language.risuNest.boot.stage}: ${failure.stage}` : '',
    ].filter((line) => line !== '').join('\n')

    const copyBootFailure = async (failure: BootFailure) => {
        const details = bootFailureDetails(failure)
        try {
            await navigator.clipboard.writeText(details)
        } catch {
            const textarea = document.createElement('textarea')
            textarea.value = details
            document.body.appendChild(textarea)
            textarea.select()
            try {
                document.execCommand('copy')
            } finally {
                document.body.removeChild(textarea)
            }
        }
        alertToast(language.risuNest.boot.copied)
    }

</script>

<ChatBindingLifecycle />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<main inert={$persistentWorkingSetInputBlocked} class="flex bg-bg w-full h-full max-w-100vw text-textcolor" use:keepFocusedInputVisible={isTauriMobile} ondragover={(e) => {
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
}} onclick={() => {
    if(keepingSessionAlive){
        return
    }

    const aliveMode = DBState?.db?.keepSessionAlive
    switch(aliveMode){
        case 'pip':{

            break
        }
        case 'sound':{
            console.log("Starting silent audio to keep session alive")
            const silentAudio = new Audio(sendSound);
            silentAudio.loop = true;
            silentAudio.volume = 0.000001;
            silentAudio.play();
            keepingSessionAlive = true;
            break
        }
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
            <div class="w-full h-full overflow-y-auto bg-darkbg text-textcolor flex justify-center items-start">
                <div class="w-full max-w-xl flex flex-col p-4 sm:p-6 gap-3">
                    <h1 class="text-xl font-bold">{language.risuNest.boot.title}</h1>
                    <p class="text-sm text-textcolor2">{bootFailureExplanation($bootFailure)}</p>
                    {#if $bootFailure.kind === 'schema-unsupported'}
                        <div class="flex flex-col gap-1 text-xs text-textcolor2 border border-darkborderc rounded-md p-3">
                            <span class="select-text break-all">{language.risuNest.boot.dataPathWindows}</span>
                            <span class="select-text break-all">{language.risuNest.boot.dataPathAndroid}</span>
                        </div>
                    {/if}
                    <code class="text-xs font-mono select-text break-all whitespace-pre-wrap border border-darkborderc rounded-md p-3 text-textcolor2">{$bootFailure.message}</code>
                    {#if $bootFailure.stage}
                        <span class="text-xs text-textcolor2 select-text">{language.risuNest.boot.stage}: {$bootFailure.stage}</span>
                    {/if}
                    {#if isTauri}
                        <div class="rounded-md border border-darkborderc p-3">
                            <p class="text-sm font-bold">{language.risuNest.recovery.exportTitle}</p>
                            <p class="mt-1 text-xs text-textcolor2">{language.risuNest.recovery.exportHelp}</p>
                            <button
                                class="mt-2 bg-darkbutton border border-darkborderc rounded-md px-4 py-2 text-sm hover:bg-selected disabled:opacity-50"
                                disabled={$bootFailure.stage === 'native-setup'}
                                onclick={exportStartupOriginalData}
                            >{language.risuNest.recovery.exportAction}</button>
                            {#if $bootFailure.stage === 'native-setup'}
                                <p class="mt-2 text-xs text-textcolor2">{language.risuNest.recovery.exportUnavailable}</p>
                            {:else if startupExportMessage}
                                <p class="mt-2 text-xs text-textcolor2" role="status">{startupExportMessage}</p>
                            {/if}
                        </div>
                    {/if}
                    <div class="flex flex-wrap gap-2 mt-1">
                        <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 text-sm hover:bg-selected" onclick={() => location.reload()}>
                            {language.risuNest.boot.restart}
                        </button>
                        <button class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 text-sm hover:bg-selected" onclick={() => copyBootFailure($bootFailure)}>
                            {language.risuNest.boot.copyDetails}
                        </button>
                    </div>
                </div>
            </div>
        {:else}
            <div
                class="w-full h-full flex justify-center items-center text-textcolor text-xl bg-darkbg"
            >
                <LoadingIndicator
                    label={language.loading}
                    detail={LoadingStatusState.text}
                    elapsedText={language.risuNest.startup.elapsed(startupElapsedSeconds)}
                />
            </div>
        {/if}
    {:else if $CustomGUISettingMenuStore}
        {#await loadCustomGUISettingMenu()}
            <div
                class="w-full h-full flex items-center justify-center text-textcolor"
            >
                <LoadingIndicator label={language.loading} />
            </div>
        {:then module}
            {@const CustomGUISettingMenu = module.default}
            <CustomGUISettingMenu />
        {:catch}
            <div
                class="w-full h-full flex flex-col gap-3 items-center justify-center text-textcolor"
                role="alert"
            >
                <span>{language.error}</span>
                <button
                    class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 hover:bg-selected"
                    onclick={() => {
                        customGUISettingMenuPromise = undefined
                        $CustomGUISettingMenuStore = false
                    }}>{language.cancel}</button
                >
            </div>
        {/await}
    {:else if !didFirstSetup || $onboardingHold}
        <Onboarding />
    {:else if $settingsOpen}
        {#await loadSettings()}
            <div
                class="w-full h-full flex items-center justify-center text-textcolor"
            >
                <LoadingIndicator label={language.loading} />
            </div>
        {:then module}
            {@const Settings = module.default}
            <Settings />
        {:catch}
            <div
                class="w-full h-full flex flex-col gap-3 items-center justify-center text-textcolor"
                role="alert"
            >
                <span>{language.error}</span>
                <button
                    class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 hover:bg-selected"
                    onclick={() => {
                        settingsPromise = undefined
                        $settingsOpen = false
                    }}>{language.cancel}</button
                >
            </div>
        {/await}
    {:else if $MobileGUI}
        <div class="w-full h-full flex flex-col">
            <MobileHeader />
            <MobileBody />
            <MobileFooter />
        </div>
    {:else}
        {#if gridOpen}
            {#await loadGridChars()}
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
                <div
                    class="w-full h-full flex flex-col gap-3 items-center justify-center text-textcolor"
                    role="alert"
                >
                    <span>{language.error}</span>
                    <button
                        class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 hover:bg-selected"
                        onclick={() => {
                            gridCharsPromise = undefined
                            gridOpen = false
                        }}>{language.cancel}</button
                    >
                </div>
            {/await}
        {:else}
            {#if (!$DynamicGUI)}
                <Sidebar openGrid={() => {gridOpen = true}} hidden={!$sideBarStore} />
            {:else}
                <div class="top-0 w-full h-full left-0 z-30 flex flex-row items-center" class:fixed={$sideBarStore} class:hidden={!$sideBarStore} >
                    <!-- svelte-ignore a11y_click_events_have_key_events -->
                    <Sidebar openGrid={() => {gridOpen = true}}  hidden={false} />



                </div>
            {/if}
            <ChatScreen />
        {/if}
    {/if}
    {#if $alertStore.type !== 'none'}
        <AlertComp />
    {/if}
    {#if $showRealmInfoStore}
        <RealmPopUp bind:openedData={$showRealmInfoStore} />
    {/if}
    {#if $ShowRealmFrameStore}
        <RealmFrame />
    {/if}
    {#if $openPresetList}
        {#await loadBotpreset()}
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
            <div
                class="absolute inset-0 z-40 flex flex-col gap-3 items-center justify-center bg-darkbg text-textcolor"
                role="alert"
            >
                <span>{language.error}</span>
                <button
                    class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 hover:bg-selected"
                    onclick={() => {
                        botpresetPromise = undefined
                        $openPresetList = false
                    }}>{language.cancel}</button
                >
            </div>
        {/await}
    {/if}
    {#if $openPersonaList}
        {#await loadListedPersona()}
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
            <div
                class="absolute inset-0 z-40 flex flex-col gap-3 items-center justify-center bg-darkbg text-textcolor"
                role="alert"
            >
                <span>{language.error}</span>
                <button
                    class="bg-darkbutton border border-darkborderc rounded-md px-4 py-2 hover:bg-selected"
                    onclick={() => {
                        listedPersonaPromise = undefined
                        $openPersonaList = false
                    }}>{language.cancel}</button
                >
            </div>
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
<SyncExitDialog />
<PersistentWorkingSetRecovery />
