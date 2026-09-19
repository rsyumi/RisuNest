import { readFileSync, readdirSync, statSync } from 'node:fs'
import { join, relative, sep } from 'node:path'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
    getDeviceMarkers,
    initializeDeviceMarkers,
    installDeviceMarkers,
} from './deviceMarkers'
import type { NativeDeviceSettings } from './nativeDeviceSettings'
import { flushDeviceSettings, loadDeviceSettings, updateDeviceSettings } from './deviceSettings'
import { reloadAppUpdateSettings, updateAppUpdateSettings } from '../update/settings'
import {
    createNativePluginPermissionStore,
    installPluginPermissionStore,
} from './nativePluginPermissions'

/**
 * Storage-name inventory, not proof of platform routing. Native persistence
 * behavior is checked separately below. Each known browser write needs an
 * explicit owner and a reason for remaining outside the native store.
 *
 * Only writes are collected: a read of a name nothing writes leaves nothing
 * behind, and a removal only takes something away.
 */
const NATIVE_RESIDUAL: Record<string, string> = {
    'localStorage risunest.windowsAppearance':
        'the window init script reads it before the page loads',
    'localStorage dynamic src/ts/storage/bootAttempt.ts':
        'risunest-boot-stage and risunest-boot-suspect are written while the native store is unavailable',
    'localStorage strongBan_*':
        'token bias recomputed from the current input',
    'localStorage risuNestServerSyncRestoreHold':
        'the device maintenance that writes it runs before any store opens',
    'indexedDB DPoPDB':
        'holds a key pair that cannot leave the browser',
    'indexedDB LLMTranslateCache':
        'translation cache, not assigned a tier',
    'indexedDB risuSaveCache':
        'export block cache, not assigned a tier',
    'indexedDB dynamic src/ts/translator/bergamotTranslator.ts':
        'local translation model cache, not assigned a tier',
    'indexedDB dynamic src/ts/drive/legacyBackupAttachments.ts':
        'temporary WebView restore staging, also used after native fallback; kept only when rollback fails',
}

/** Reached only by the web build, which has no device tier. */
const WEB_ONLY: Record<string, string> = {
    'localStorage fallbackRisuToken': 'a native install holds the token in the vault',
    'localStorage risuauth': 'the node server build signs in without a vault',
    'localStorage dynamic src/ts/sionyw.ts': 'the insecure file fallback replaces native files',
    'localStorage dynamic src/ts/storage/deviceBackup/scopes.ts':
        'restores the keys it captured from the same storage',
    'indexedDB risunest': 'the browser data store and its migration',
    'indexedDB hypaVector': 'the browser embedding cache',
    'indexedDB plugin_permissions': 'the browser plugin consent store',
    'indexedDB plugin': 'the browser plugin device keyspace',
    'localStorage dynamic src/ts/plugins/pluginDeviceKeyspace.ts':
        'the browser backend addresses one keyspace out of an opaque key',
    'indexedDB risuaiSyncConflictBackup': 'the browser conflict backup store',
    'indexedDB dynamic src/ts/storage/indexedDbPersistentDataStore.ts':
        'the browser data store opens the name it was built with',
    'indexedDB dynamic src/ts/storage/deviceBackup/indexedDb.ts':
        'inspects the databases the browser already has',
}

const sourceRoot = join(process.cwd(), 'src')

function sourceFiles(directory: string): string[] {
    const found: string[] = []
    for (const entry of readdirSync(directory)) {
        const full = join(directory, entry)
        if (statSync(full).isDirectory()) {
            if (entry === 'tests' || entry === '__snapshots__') continue
            found.push(...sourceFiles(full))
            continue
        }
        if (!/\.(ts|svelte)$/.test(entry)) continue
        if (/\.(test|bench)\.ts$/.test(entry)) continue
        found.push(full)
    }
    return found
}

/** Returns the call argument text that starts right after `(`. */
function argumentText(source: string, start: number): string {
    let depth = 0
    for (let index = start; index < source.length; index += 1) {
        const character = source[index]
        if (character === '(' || character === '[' || character === '{') depth += 1
        else if (character === ')' || character === ']' || character === '}') {
            if (depth === 0) return source.slice(start, index)
            depth -= 1
        } else if (character === ',' && depth === 0) return source.slice(start, index)
    }
    return source.slice(start)
}

function keyOf(argument: string, file: string, source = ''): string {
    const trimmed = argument.trim()
    if (/^[A-Za-z_$][\w$]*$/.test(trimmed)) {
        const declared = new RegExp(
            String.raw`const\s+${trimmed}\s*=\s*(['"])(.*?)\1`,
            's',
        ).exec(source)
        if (declared) return declared[2]
    }
    const quoted = /^(['"])(.*?)\1$/s.exec(trimmed)
    if (quoted) return quoted[2]
    const template = /^`([^`$]*)\$\{/s.exec(trimmed)
    if (template) return `${template[1]}*`
    const plain = /^`([^`$]*)`$/s.exec(trimmed)
    if (plain) return plain[1]
    const concatenated = /^(['"])(.*?)\1\s*\+/s.exec(trimmed)
    if (concatenated) return `${concatenated[2]}*`
    return `dynamic ${file}`
}

function collect(): Set<string> {
    const found = new Set<string>()
    for (const path of sourceFiles(sourceRoot)) {
        const file = relative(process.cwd(), path).split(sep).join('/')
        const source = readFileSync(path, 'utf8')
        for (const match of source.matchAll(/localStorage\s*\.\s*setItem\s*\(/g)) {
            const argument = argumentText(source, match.index + match[0].length)
            found.add(`localStorage ${keyOf(argument, file, source)}`)
        }
        for (const match of source.matchAll(/localforage\s*\.\s*createInstance\s*\(/g)) {
            const argument = argumentText(source, match.index + match[0].length)
            const name = /name\s*:\s*(['"])(.*?)\1/s.exec(argument)
            found.add(`indexedDB ${name ? name[2] : `dynamic ${file}`}`)
        }
        for (const match of source.matchAll(/indexedDB\s*\.\s*open\s*\(|indexedDbFactory\s*\.\s*open\s*\(/gi)) {
            const argument = argumentText(source, match.index + match[0].length)
            found.add(`indexedDB ${keyOf(argument, file, source)}`)
        }
    }
    return found
}

describe('browser storage-name inventory', () => {
    it('accounts for every source-level storage name with an owner and rationale', () => {
        const allowed = [...Object.keys(NATIVE_RESIDUAL), ...Object.keys(WEB_ONLY)].sort()

        expect([...collect()].sort()).toEqual(allowed)
    })

    it('states why every allowed name stays in the browser', () => {
        for (const reason of [...Object.values(NATIVE_RESIDUAL), ...Object.values(WEB_ONLY)]) {
            expect(reason.length).toBeGreaterThan(0)
        }
    })
})

describe('a native install after the flows that used to write to the browser', () => {
    const stored = new Map<string, unknown>()
    const settings: NativeDeviceSettings = {
        get: async (key) => stored.get(key) ?? null,
        readMany: async (keys) => keys.map((key) => stored.get(key) ?? null),
        set: async (key, value) => {
            if (value === null) stored.delete(key)
            else stored.set(key, value)
        },
        patch: async () => undefined,
    }
    const host = globalThis as unknown as Record<'indexedDB', unknown>
    let previousFactory: unknown

    beforeEach(() => {
        stored.clear()
        localStorage.clear()
        previousFactory = host.indexedDB
        host.indexedDB = {
            open(name: string) {
                throw new Error(`a native install opened the ${name} database`)
            },
        }
    })

    afterEach(() => {
        host.indexedDB = previousFactory
        installDeviceMarkers(null)
        installPluginPermissionStore(null)
    })

    it('leaves nothing behind after loading, settings changes, account markers and consent', async () => {
        const markers = await initializeDeviceMarkers(settings)
        loadDeviceSettings(markers)

        updateDeviceSettings({ performanceProfile: 'low-spec', startupExclusions: ['plugins'] })
        await flushDeviceSettings()
        updateAppUpdateSettings({ autoUpdateCheck: false })
        await getDeviceMarkers().flush()

        getDeviceMarkers().setItem('accountst', 'able')
        getDeviceMarkers().setItem('dosync', 'sync')
        await getDeviceMarkers().flush()
        getDeviceMarkers().removeItem('accountst')
        getDeviceMarkers().setItem('dosync', 'avoid')
        await getDeviceMarkers().flush()

        const written: string[] = []
        const permissions = createNativePluginPermissionStore(
            vi.fn(async (command: string) => {
                written.push(command)
                return command === 'pds_read_plugin_permissions'
                    ? { permissions: [], grants: [] }
                    : undefined
            }),
        )
        installPluginPermissionStore(permissions)
        await permissions.grant('hash-a', 'db')
        await permissions.recordGrant('Plugin A', 'db', 3)
        await expect(permissions.isGranted('hash-a', 'db')).resolves.toBe(true)

        expect(localStorage.length).toBe(0)
        expect([...stored.keys()].sort()).toEqual([
            'dosync', 'risuNestDeviceSettings', 'risuNestUpdateSettings',
        ])
        expect(written).toContain('pds_write_plugin_permission')

        // Reading them back again must not fall through to the browser either.
        installDeviceMarkers(null)
        loadDeviceSettings(await initializeDeviceMarkers(settings))
        expect(reloadAppUpdateSettings().autoUpdateCheck).toBe(false)
        expect(localStorage.length).toBe(0)
    })
})
