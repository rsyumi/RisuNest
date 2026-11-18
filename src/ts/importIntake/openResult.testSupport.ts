// A `src/ts/stores.svelte` mock for tests of the import screens, which cuts the app's import graph:
//   vi.mock('src/ts/stores.svelte', async () => (await import('src/ts/importIntake/openResult.testSupport')).importStoresModule())

type Stores = typeof import('src/ts/stores.svelte')

/** The stores `openImportResult` reads and changes, plus the alert store `modalNavigation` reads. */
export async function importStoresModule() {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: { characters: [] } } as unknown as Stores['DBState'],
        alertStore: writable({ type: 'none', msg: '' }) as unknown as Stores['alertStore'],
        bookmarkListOpen: writable(false),
        botMakerMode: writable(false),
        CharConfigSubMenu: writable(0),
        customSideBarConfigDialogStore: { open: false },
        easyPanelStore: { open: false },
        hypaV3ModalOpen: writable(false),
        irisStore: { open: false },
        loadoutModalStore: { open: false },
        MobileGUI: writable(false),
        MobileSideBar: writable(0),
        moduleScopeMenuOpen: writable(false),
        openPersonaList: writable(false),
        openPresetList: writable(false),
        popUpEditorStore: { open: false, value: '', mode: 'default', language: 'markdown' },
        QuickSettings: { open: false, index: 0 },
        settingsOpen: writable(false),
        SettingsMenuIndex: writable(-1),
        ShowRealmFrameStore: writable(''),
        sideBarStore: writable(false),
    } satisfies Partial<Stores>
}
