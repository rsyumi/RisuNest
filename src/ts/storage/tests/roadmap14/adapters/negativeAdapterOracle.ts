import negativeFixtureArtifact from './fixtures/negative/manifest.json?raw'

type NegativeFixtureId =
    | 'stale-block-cache'
    | 'invalid-required-block'
    | 'truncated-local-backup-tail'
    | 'payload-before-database'
    | 'charx-extension-path-collisions'

type NegativeFixture = {
    id: NegativeFixtureId
    adapter: string
    inputBase64?: string
    paths?: string[]
    warning: string
}

type NegativeFixtureManifest = {
    version: number
    fixtures: NegativeFixture[]
}

type RisuSaveDecoder = (bytes: Uint8Array) => Promise<unknown>

const manifest = JSON.parse(negativeFixtureArtifact) as NegativeFixtureManifest
if (manifest.version !== 1) throw new Error(`Unsupported negative fixture version: ${manifest.version}`)

function fixture(id: NegativeFixtureId): NegativeFixture {
    const value = manifest.fixtures.find((candidate) => candidate.id === id)
    if (!value) throw new Error(`Missing negative adapter fixture: ${id}`)
    return value
}

function fixtureBytes(id: NegativeFixtureId): Uint8Array {
    const inputBase64 = fixture(id).inputBase64
    if (!inputBase64) throw new Error(`Negative fixture has no byte input: ${id}`)
    return Uint8Array.from(Buffer.from(inputBase64, 'base64'))
}

export async function observeRisuSaveNegativeFixture(
    id: 'stale-block-cache' | 'invalid-required-block',
    decode: RisuSaveDecoder,
): Promise<{
    fixtureId: typeof id
    status: 'known-gap' | 'passing'
    observed: string
    warning?: string
}> {
    const value = fixture(id)
    try {
        const decoded = await decode(fixtureBytes(id)) as {
            characters?: { chaId?: string }[]
        }
        if (id === 'stale-block-cache') {
            const loadedStaleBlock = decoded.characters?.some((character) =>
                character.chaId === 'char-stale') ?? false
            return loadedStaleBlock
                ? {
                    fixtureId: id,
                    status: 'known-gap',
                    observed: 'missing block was completed from stale local cache',
                    warning: value.warning,
                }
                : {
                    fixtureId: id,
                    status: 'known-gap',
                    observed: 'missing required block was silently skipped',
                    warning: value.warning,
                }
        }

        const acceptedInvalidBlock = decoded.characters?.some((character) =>
            character.chaId === 'char-invalid')
        return {
            fixtureId: id,
            status: 'known-gap',
            observed: acceptedInvalidBlock
                ? 'invalid required character block was accepted'
                : 'invalid required character block was skipped',
            warning: value.warning,
        }
    } catch {
        return {
            fixtureId: id,
            status: 'passing',
            observed: id === 'stale-block-cache'
                ? 'missing required block was rejected'
                : 'invalid required character block was rejected',
        }
    }
}

function readUint32(bytes: Uint8Array, offset: number): number {
    return new DataView(bytes.buffer, bytes.byteOffset + offset, 4).getUint32(0, true)
}

export function inspectLocalBackupFixture(
    id: 'truncated-local-backup-tail' | 'payload-before-database',
): {
    evidence: 'fixture-only'
    entries: string[]
    trailingByteLength: number
    payloadBeforeDatabase: boolean
    result: { status: 'unprobed-known-gap'; warning: string }
} {
    const value = fixture(id)
    const bytes = fixtureBytes(id)
    const entries: string[] = []
    let offset = 0
    while (offset + 4 <= bytes.byteLength) {
        const entryOffset = offset
        const nameLength = readUint32(bytes, offset)
        offset += 4
        if (offset + nameLength + 4 > bytes.byteLength) {
            offset = entryOffset
            break
        }
        const name = new TextDecoder().decode(bytes.subarray(offset, offset + nameLength))
        offset += nameLength
        const dataLength = readUint32(bytes, offset)
        offset += 4
        if (offset + dataLength > bytes.byteLength) {
            offset = entryOffset
            break
        }
        entries.push(name)
        offset += dataLength
    }

    const databaseIndex = entries.indexOf('database.risudat')
    return {
        evidence: 'fixture-only',
        entries,
        trailingByteLength: bytes.byteLength - offset,
        payloadBeforeDatabase: databaseIndex > 0 && entries
            .slice(0, databaseIndex)
            .some((name) => name !== 'encryption.risudat'),
        result: { status: 'unprobed-known-gap', warning: value.warning },
    }
}

function extensionOf(path: string): string | null {
    const name = path.split('/').at(-1) ?? ''
    const separator = name.lastIndexOf('.')
    return separator <= 0 || separator === name.length - 1
        ? null
        : name.slice(separator + 1).toLowerCase()
}

function collisionKey(path: string): string | null {
    const segments = path.replaceAll('\\', '/').split('/')
    if (segments.some((segment) => segment === '.' || segment === '..')) return null
    return segments.map((segment) => segment
        .replace(/[<>:"|?*\x00-\x1F]/g, '_')
        .replace(/[. ]+$/, '')
        .toLowerCase(),
    ).join('/')
}

export function inspectCharXCollisionFixture(): {
    evidence: 'fixture-only'
    collisionGroups: string[][]
    extensions: (string | null)[]
    unsafePaths: string[]
    result: { status: 'unprobed-known-gap'; warning: string }
} {
    const value = fixture('charx-extension-path-collisions')
    const paths = value.paths ?? []
    const unsafePaths = paths.filter((path) => collisionKey(path) === null)
    const byKey = new Map<string, string[]>()
    for (const path of paths) {
        const key = collisionKey(path)
        if (key === null) continue
        const group = byKey.get(key) ?? []
        group.push(path)
        byKey.set(key, group)
    }
    const extensions = [...new Set(paths.map(extensionOf))]
        .sort((left, right) => left === null ? -1 : right === null ? 1 : left.localeCompare(right))

    return {
        evidence: 'fixture-only',
        collisionGroups: [...byKey.values()].filter((group) => group.length > 1),
        extensions,
        unsafePaths,
        result: { status: 'unprobed-known-gap', warning: value.warning },
    }
}
