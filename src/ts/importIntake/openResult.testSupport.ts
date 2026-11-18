// A `src/ts/stores.svelte` mock for tests of the import screens, which cuts the app's import graph:
//   vi.mock('src/ts/stores.svelte', async () => (await import('src/ts/importIntake/openResult.testSupport')).importStoresModule())

type Stores = typeof import('src/ts/stores.svelte')

/** The stores `openImportResult` reads and changes, plus the alert store `modalNavigation` reads. */
export async function importStoresModule() {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: { characters: [] } } as unknown as Stores['DBState'],
        alertStore: writable({ type: 'none', msg: '' }) as unknown as Stores['alertStore'],
        botMakerMode: writable(false),
        CharConfigSubMenu: writable(0),
        MobileGUI: writable(false),
        MobileSideBar: writable(0),
        openPersonaList: writable(false),
        openPresetList: writable(false),
        QuickSettings: { open: false, index: 0 },
        settingsOpen: writable(false),
        SettingsMenuIndex: writable(-1),
        sideBarStore: writable(false),
    } satisfies Partial<Stores>
}
