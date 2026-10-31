import { get, writable } from 'svelte/store'
import { alertStore } from './modalNavigationAdapters'
export { languageKorean as language } from '../../src/lang/ko'

export const DBState = { db: { didFirstSetup: true, characters: [{ name: 'Synthetic', chaId: 'character-a', chatPage: 0, chats: [{ id: 'chat-a' }] }] } }
export const DynamicGUI = writable(false), settingsOpen = writable(false), sideBarStore = writable(true), sideBarClosing = writable(false)
export const ShowRealmFrameStore = writable(false), openPresetList = writable(false), openPersonaList = writable(false)
export const MobileGUI = writable(false), CustomGUISettingMenuStore = writable(false), loadedStore = writable(!new URLSearchParams(location.search).has('loading'))
export const bootFailure = writable<import('../../src/ts/stores.svelte').BootFailure | null>(null)
export const recoveryStart = writable(null)
export const serverSyncNavigation = { subscribe: (_listener: unknown) => () => {} }
export const dataHealthNavigation = serverSyncNavigation
export const getStartupExclusions = () => ['autoUpdate']
export const isStartupExcluded = () => true
export { alertStore }
export { showsDialog } from '../../src/ts/alertQueue'
export const bookmarkListOpen = writable(false)
export const LoadingStatusState = { startedAt: null, text: '' }
export const popupStore = {}, easyPanelStore = {}, popUpEditorStore = {}, loadoutModalStore = {}, onboardingModalStore = {}, irisStore = {}, customSideBarConfigDialogStore = {}
export const hypaV3ModalOpen = writable(false), hypaV3ProgressStore = writable({ open: false })
export const MobileGUIStack = writable(1), MobileSideBar = writable(0), selectedCharID = writable(-1)
export const onboardingHold = writable(false), showRealmInfoStore = writable(null), SettingsMenuIndex = writable(0)
export const MobileSearch = writable(''), additionalSettingsMenu = []
export const persistentWorkingSetInputBlocked = writable(false), navigationActivity = writable(null), isLite = writable(false)
export const keepFocusedInputVisible = () => {}, restoreFocusAfterInputBlock = () => {}
export { modalNavigation, backNavigationLayer, handleRootBack } from '../../src/ts/ui/modalNavigation'
export const textEditorPopup = { request: null }

let failingRoute = ''
let attempts = 0
let holdFirst = false
let rejectHeld: (() => void) | undefined
export const lazyImportBoundary = {
    arm(route: string) { failingRoute = route === 'mobile' ? 'settings' : route; attempts = 0; holdFirst = false },
    hold(route: string) { this.arm(route); holdFirst = true },
    rejectHeld() { rejectHeld!() },
    attempts() { return attempts },
    async load(route: string, load: () => Promise<unknown>) {
        if (route === failingRoute && ++attempts === 1) {
            if (!holdFirst) throw new Error('Synthetic one-shot import rejection')
            await new Promise<void>((_resolve, reject) => { rejectHeld = () => reject(new Error('Synthetic stale import rejection')) })
        }
        return load()
    },
}

export function openRoute(route: string) {
    if (route === 'settings') settingsOpen.set(true)
    else if (route === 'custom') CustomGUISettingMenuStore.set(true)
    else if (route === 'presets') openPresetList.set(true)
    else if (route === 'personas') openPersonaList.set(true)
    else if (route === 'mobile') { MobileGUI.set(true); MobileGUIStack.set(2) }
    else if (route === 'mobile-chat') { MobileGUI.set(true); selectedCharID.set(0); SettingsMenuIndex.set(-1) }
    else if (route === 'sidebar') { DynamicGUI.set(true); sideBarStore.set(true) }
    else throw new Error(`Unknown synthetic route: ${route}`)
}

export const navigationState = {
    layout(mobile: boolean) { MobileGUI.set(mobile) },
    read() { return { sidebarClosing: get(sideBarClosing), entries: history.state?.risunestModal?.length ?? 0 } },
}
