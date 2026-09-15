import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import { stat } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { assertSyntheticProfile } from './fixture.mjs'

const execute = promisify(execFile)

export async function observeWal(root, identifier) {
    assertSyntheticProfile(root, identifier)
    const file = path.join(root, 'persistent/persistent.sqlite-wal')
    const read = async () => {
        try {
            return (await stat(file)).size
        } catch (error) {
            if (error.code === 'ENOENT') return 0
            throw error
        }
    }
    const startBytes = await read()
    let peakBytes = startBytes
    let samples = 1
    let failure
    let result
    let pending = Promise.resolve()
    const timer = setInterval(() => {
        pending = pending.then(async () => {
            try {
                peakBytes = Math.max(peakBytes, await read())
                samples++
            } catch (error) {
                failure = error
            }
        })
    }, 100)
    return async () => {
        if (result) return result
        clearInterval(timer)
        await pending
        if (failure) throw failure
        const endBytes = await read()
        result = {
            startBytes,
            endBytes,
            peakBytes: Math.max(peakBytes, endBytes),
            samples: samples + 1,
        }
        return result
    }
}

export async function readWindowsMemory(pid) {
    if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error('Invalid benchmark PID')
    const { stdout } = await execute(
        'powershell.exe',
        [
            '-NoProfile',
            '-NonInteractive',
            '-File',
            fileURLToPath(new URL('./host-memory.ps1', import.meta.url)),
            '-RootProcessId',
            String(pid),
        ],
        { windowsHide: true, timeout: 15_000 },
    )
    const raw = JSON.parse(stdout)
    const result = {}
    for (const key of [
        'nativeWorkingSetBytes',
        'nativePeakWorkingSetBytes',
        'treeWorkingSetBytes',
        'processCount',
    ]) {
        if (!Number.isSafeInteger(raw[key]) || raw[key] < 0)
            throw new Error('Invalid memory sample')
        result[key] = raw[key]
    }
    return result
}
