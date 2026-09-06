import { describe, expect, it } from 'vitest'
import { cutDeviceNetwork, restoreDeviceNetwork } from './phase3AndroidSmoke.mjs'

type AdbResult = { status: number; stdout: string; stderr?: string }

function fakeAdb(script: (args: string[]) => AdbResult) {
    const calls: string[][] = []
    const run = (_serial: string, args: string[], options: { allowFailure?: boolean } = {}) => {
        calls.push(args)
        const result = script(args)
        if (result.status !== 0 && !options.allowFailure) {
            throw new Error(`adb ${args.join(' ')} failed (${result.status})`)
        }
        return result
    }
    return { run, calls }
}

const ok = { status: 0, stdout: '' }
const noSleep = async () => {}

describe('cutDeviceNetwork', () => {
    it('turns the radios off and returns once no default network remains', async () => {
        const { run, calls } = fakeAdb((args) =>
            args.includes('dumpsys') ? { status: 0, stdout: 'Active default network: none\n' } : ok,
        )
        await cutDeviceNetwork('emulator-5554', { run, sleep: noSleep })
        expect(calls).toContainEqual(['shell', 'cmd', 'connectivity', 'airplane-mode', 'enable'])
        expect(calls).toContainEqual(['shell', 'svc', 'wifi', 'disable'])
        expect(calls).toContainEqual(['shell', 'svc', 'data', 'disable'])
    })

    it('refuses to continue while a default network is still active', async () => {
        const { run } = fakeAdb((args) =>
            args.includes('dumpsys') ? { status: 0, stdout: 'Active default network: 100\n' } : ok,
        )
        await expect(
            cutDeviceNetwork('emulator-5554', { run, sleep: noSleep, attempts: 2 }),
        ).rejects.toThrow(/still reachable/)
    })

    it('waits for the network to tear down before deciding', async () => {
        let polls = 0
        const { run } = fakeAdb((args) => {
            if (!args.includes('dumpsys')) return ok
            polls += 1
            return { status: 0, stdout: polls < 3 ? 'Active default network: 100\n' : 'Active default network: none\n' }
        })
        await cutDeviceNetwork('emulator-5554', { run, sleep: noSleep, attempts: 5 })
        expect(polls).toBe(3)
    })

    it('refuses when the device cannot report its network state', async () => {
        const { run } = fakeAdb((args) => (args.includes('dumpsys') ? { status: 0, stdout: 'unexpected output' } : ok))
        await expect(
            cutDeviceNetwork('emulator-5554', { run, sleep: noSleep, attempts: 1 }),
        ).rejects.toThrow(/could not verify/)
    })

    it('does not fail just because an older device lacks one of the radio commands', async () => {
        const { run } = fakeAdb((args) => {
            if (args.includes('dumpsys')) return { status: 0, stdout: 'Active default network: none\n' }
            if (args.includes('airplane-mode')) return { status: 255, stdout: '', stderr: 'unknown command' }
            return ok
        })
        await expect(cutDeviceNetwork('emulator-5554', { run, sleep: noSleep })).resolves.toBeUndefined()
    })
})

describe('restoreDeviceNetwork', () => {
    it('turns airplane mode off and the radios back on', () => {
        const { run, calls } = fakeAdb(() => ok)
        restoreDeviceNetwork('emulator-5554', { run })
        expect(calls).toEqual([
            ['shell', 'cmd', 'connectivity', 'airplane-mode', 'disable'],
            ['shell', 'svc', 'wifi', 'enable'],
            ['shell', 'svc', 'data', 'enable'],
        ])
    })
})
