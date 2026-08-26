import { createHash } from 'node:crypto'
import blockExpectedCanonicalArtifact from './fixtures/legacy/risusave-block-v4.expected.json?raw'
import blockInputArtifact from './fixtures/legacy/risusave-block-v4.input.base64?raw'
import compressedInputArtifact from './fixtures/legacy/risusave-compressed-v4.input.base64?raw'
import expectedCanonicalArtifact from './fixtures/legacy/risusave-raw-v4.expected.json?raw'
import manifestArtifact from './fixtures/legacy/manifest.json?raw'
import rawInputArtifact from './fixtures/legacy/risusave-raw-v4.input.base64?raw'
import streamInputArtifact from './fixtures/legacy/risusave-stream-v4.input.base64?raw'
import warningArtifact from './fixtures/legacy/risusave-raw-v4.warnings.json?raw'

type FrozenLegacyInput = {
    artifactId: string
    input: string
    inputByteLength: number
    inputSha256: string
    expectedCanonical?: string
    expectedCanonicalSha256?: string
}

type FrozenLegacyManifest = {
    version: number
    compatibilityBaseline: string
    expectedCanonical: string
    expectedCanonicalSha256: string
    warnings: string
    artifacts: FrozenLegacyInput[]
}

type RisuSaveDecoder = (bytes: Uint8Array) => Promise<unknown>

function sha256(value: Uint8Array | string): string {
    return createHash('sha256').update(value).digest('hex')
}

const inputArtifacts: Record<string, string> = {
    'risusave-raw-v4.input.base64': rawInputArtifact,
    'risusave-compressed-v4.input.base64': compressedInputArtifact,
    'risusave-stream-v4.input.base64': streamInputArtifact,
    'risusave-block-v4.input.base64': blockInputArtifact,
}

const canonicalArtifacts: Record<string, string> = {
    'risusave-raw-v4.expected.json': expectedCanonicalArtifact,
    'risusave-block-v4.expected.json': blockExpectedCanonicalArtifact,
}

export async function verifyFrozenLegacyArtifacts(decode: RisuSaveDecoder): Promise<{
    artifactId: string
    status: 'passing'
    warnings: string[]
}[]> {
    const manifest = JSON.parse(manifestArtifact) as FrozenLegacyManifest
    if (manifest.version !== 1) throw new Error(`Unsupported frozen oracle version: ${manifest.version}`)
    if (
        manifest.expectedCanonical !== 'risusave-raw-v4.expected.json' ||
        manifest.warnings !== 'risusave-raw-v4.warnings.json'
    ) {
        throw new Error('Frozen legacy manifest references an unknown output artifact')
    }

    if (sha256(expectedCanonicalArtifact) !== manifest.expectedCanonicalSha256) {
        throw new Error('Frozen legacy canonical artifact changed')
    }
    const warnings = JSON.parse(warningArtifact) as string[]
    const results = []
    for (const artifact of manifest.artifacts) {
        const encodedInput = inputArtifacts[artifact.input]
        if (encodedInput === undefined) {
            throw new Error(`Frozen legacy manifest references an unknown input: ${artifact.artifactId}`)
        }
        const input = Uint8Array.from(Buffer.from(encodedInput.trim(), 'base64'))
        if (input.byteLength !== artifact.inputByteLength || sha256(input) !== artifact.inputSha256) {
            throw new Error(`Frozen legacy input artifact changed: ${artifact.artifactId}`)
        }

        const canonicalName = artifact.expectedCanonical ?? manifest.expectedCanonical
        const canonicalHash = artifact.expectedCanonicalSha256 ?? manifest.expectedCanonicalSha256
        const expectedCanonical = canonicalArtifacts[canonicalName]
        if (expectedCanonical === undefined || sha256(expectedCanonical) !== canonicalHash) {
            throw new Error(`Frozen legacy canonical artifact changed: ${artifact.artifactId}`)
        }
        const decoded = await decode(input)
        const actualCanonical = `${JSON.stringify(decoded, null, 2)}\n`
        if (actualCanonical !== expectedCanonical) {
            throw new Error(`Legacy reverse import changed: ${artifact.artifactId}`)
        }
        results.push({ artifactId: artifact.artifactId, status: 'passing' as const, warnings })
    }
    return results
}
