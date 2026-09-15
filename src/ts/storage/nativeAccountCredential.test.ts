import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
    isTauri: true,
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('../platform', () => ({
    get isTauri() {
        return mocks.isTauri
    },
}))

import { createNativeAccountCredentialVault } from './nativeAccountCredential'

describe('native account credential vault', () => {
    beforeEach(() => {
        mocks.invoke.mockReset()
        mocks.isTauri = true
    })

    it('maps read, write, and clear to the vault commands', async () => {
        mocks.invoke.mockResolvedValueOnce({ id: 'account-1', token: 'stored-token' })
            .mockResolvedValue(undefined)
        const vault = createNativeAccountCredentialVault()

        await expect(vault.read()).resolves.toEqual({ id: 'account-1', token: 'stored-token' })
        await vault.write({ id: 'account-1', token: 'rotated-token' })
        await vault.clear()

        expect(mocks.invoke.mock.calls).toEqual([
            ['account_credential_read'],
            ['account_credential_write', {
                credential: { id: 'account-1', token: 'rotated-token' },
            }],
            ['account_credential_clear'],
        ])
    })

    it('reports an empty vault as no credential', async () => {
        mocks.invoke.mockResolvedValueOnce(null)
        await expect(createNativeAccountCredentialVault().read()).resolves.toBeNull()
    })

    it('rejects every operation outside Tauri', async () => {
        mocks.isTauri = false
        const vault = createNativeAccountCredentialVault()

        await expect(vault.read()).rejects.toThrow('The account credential vault requires Tauri')
        await expect(vault.write({})).rejects.toThrow(
            'The account credential vault requires Tauri',
        )
        await expect(vault.clear()).rejects.toThrow('The account credential vault requires Tauri')
        expect(mocks.invoke).not.toHaveBeenCalled()
    })
})
