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
