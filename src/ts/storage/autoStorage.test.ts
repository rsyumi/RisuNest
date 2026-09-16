import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => {
    const localBackend = {
        setItem: vi.fn(async () => undefined),
        getItem: vi.fn(async () => new Uint8Array([1, 2, 3])),
        keys: vi.fn(async () => ['database/database.bin']),
        removeItem: vi.fn(async () => undefined),
    }

    return {
        localBackend,
        createInstance: vi.fn(() => localBackend),
        clearDefaultStore: vi.fn(async () => undefined),
        accountConstructed: vi.fn(),
        accountGetItem: vi.fn(async () => null),
        accountSetItem: vi.fn(async (key: string) => key),
        getDatabase: vi.fn(() => ({
            formatversion: 4,
            characters: [],
            account: { token: 'synthetic-token' },
        })),
        alertInput: vi.fn(async () => 'RISUNEST'),
        collectColdStorageBackupPayloads: vi.fn(async () => ({
            payloads: [],
            missingKeys: [],
            invalidKeys: [],
        })),
    }
})

vi.mock('localforage', () => ({
    default: {
        createInstance: mocks.createInstance,
        clear: mocks.clearDefaultStore,
    },
}))

vi.mock('../globalApi.svelte', () => ({
    replaceDbResources: (value: unknown) => value,
}))

vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))
vi.mock('./nodeStorage', () => ({ NodeStorage: class {} }))
vi.mock('./opfsStorage', () => ({ OpfsStorage: class {} }))

vi.mock('../alert', () => ({
    alertError: vi.fn(),
    alertInput: mocks.alertInput,
    alertSelect: vi.fn(async () => '1'),
    alertStore: { set: vi.fn() },
}))

vi.mock('./database.svelte', () => ({
    getDatabase: mocks.getDatabase,
}))

vi.mock('./accountStorage', () => ({
    AccountStorage: class {
        constructor() {
            mocks.accountConstructed()
        }

        getItem = mocks.accountGetItem
        setItem = mocks.accountSetItem
    },
}))

vi.mock('./risuSave', () => ({
    encodeRisuSaveLegacy: () => new Uint8Array([7, 8, 9]),
    decodeRisuSave: async () => ({ formatversion: 4, characters: [] }),
}))

vi.mock('src/lang', () => ({
    language: {
        loadDataFromAccount: 'load',
        saveCurrentDataToAccount: 'save',
    },
}))

vi.mock('../process/coldstorage.svelte', () => ({
    collectColdStorageBackupPayloads: mocks.collectColdStorageBackupPayloads,
    replaceColdStoragePayloadResources: (value: unknown) => value,
    setAccountColdStorageItem: vi.fn(async () => true),
}))

import { AutoStorage } from './autoStorage'

describe('AutoStorage account mode separation', () => {
    beforeEach(() => {
        localStorage.clear()
        vi.clearAllMocks()
        mocks.createInstance.mockReturnValue(mocks.localBackend)
        mocks.localBackend.getItem.mockResolvedValue(new Uint8Array([1, 2, 3]))
        mocks.localBackend.keys.mockResolvedValue(['database/database.bin'])
        mocks.accountGetItem.mockResolvedValue(null)
        mocks.accountSetItem.mockImplementation(async (key: string) => key)
    })

    it('keeps account mode as a marker while database and asset operations use local storage', async () => {
        localStorage.setItem('accountst', 'able')
        const storage = new AutoStorage()

        await storage.setItem('database/database.bin', new Uint8Array([4]))
        await storage.setItem('assets/synthetic.png', new Uint8Array([5]))
        await storage.getItem('database/database.bin')
        await storage.getItem('assets/synthetic.png')

        expect(storage.isAccount).toBe(true)
        expect(storage.realStorage).toBe(mocks.localBackend)
        expect(mocks.accountConstructed).not.toHaveBeenCalled()
        expect(mocks.localBackend.setItem).toHaveBeenNthCalledWith(
            1,
            'database/database.bin',
            new Uint8Array([4]),
        )
        expect(mocks.localBackend.setItem).toHaveBeenNthCalledWith(
            2,
            'assets/synthetic.png',
            new Uint8Array([5]),
        )
        expect(mocks.localBackend.getItem).toHaveBeenNthCalledWith(1, 'database/database.bin')
        expect(mocks.localBackend.getItem).toHaveBeenNthCalledWith(2, 'assets/synthetic.png')
    })

    it('keeps a current-session local override across repeated initialization', async () => {
        localStorage.setItem('accountst', 'able')
        const storage = new AutoStorage()
        await storage.Init()
        expect(storage.isAccount).toBe(true)

        storage.setAccountModeForSession(false)
        await storage.Init()

        expect(localStorage.getItem('accountst')).toBe('able')
        expect(storage.isAccount).toBe(false)
        expect(storage.realStorage).toBe(mocks.localBackend)
    })

})
