import type { BidirectionalSyncConflictPlan } from './bidirectionalSyncPlan'

const SHA256_PATTERN = /^[0-9a-f]{64}$/
const REQUIRED_COMPONENTS = ['assets', 'cold', 'database', 'inlays'] as const
const MAX_PACKAGE_ID_BYTES = 1024
const textEncoder = new TextEncoder()

export type LosslessBackupComponent = typeof REQUIRED_COMPONENTS[number]

class VerifiedLosslessBackupProofValue {
    readonly #j2Verified = true

    private constructor(
        readonly packageId: string,
        readonly libraryId: string,
        readonly sourceManifestHash: string,
        readonly verifiedComponents: readonly LosslessBackupComponent[],
    ) {}
}

export type VerifiedLosslessBackupProof = VerifiedLosslessBackupProofValue

export interface ConflictReplacementRequest {
    plan: BidirectionalSyncConflictPlan
    winner: 'local' | 'remote'
    backup: VerifiedLosslessBackupProof | undefined
}

export type ConflictReplacementAuthorization =
    | {
          kind: 'blocked'
          reason: 'lossless-backup-required'
          losingSide: 'local' | 'remote'
          requiredManifestHash: string
      }
    | {
          kind: 'authorized'
          winner: 'local' | 'remote'
          losingSide: 'local' | 'remote'
          packageId: string
          sourceManifestHash: string
      }

function validatePackageId(value: unknown): string {
    if (
        typeof value !== 'string'
        || value.length === 0
        || textEncoder.encode(value).byteLength > MAX_PACKAGE_ID_BYTES
    ) {
        throw new TypeError('Lossless backup package id must be a bounded nonempty string')
    }
    return value
}

function validateComponents(value: readonly LosslessBackupComponent[]): void {
    if (!Array.isArray(value)) throw new TypeError('Lossless backup components are invalid')
    const components = [...value].sort()
    if (
        components.length !== REQUIRED_COMPONENTS.length
        || components.some((component, index) => component !== REQUIRED_COMPONENTS[index])
    ) {
        throw new TypeError('Lossless backup components must include database, assets, inlays, and cold')
    }
}

export function authorizeConflictReplacement(
    request: ConflictReplacementRequest,
): ConflictReplacementAuthorization {
    if (request.plan.kind !== 'conflict') {
        throw new TypeError('Conflict replacement requires a conflict plan')
    }
    if (request.winner !== 'local' && request.winner !== 'remote') {
        throw new TypeError('Conflict replacement winner is invalid')
    }
    const losingSide = request.winner === 'local' ? 'remote' : 'local'
    const requiredManifestHash = losingSide === 'local'
        ? request.plan.localManifestHash
        : request.plan.remoteManifestHash
    if (!request.backup) {
        return {
            kind: 'blocked',
            reason: 'lossless-backup-required',
            losingSide,
            requiredManifestHash,
        }
    }
    if (!(request.backup instanceof VerifiedLosslessBackupProofValue)) {
        throw new TypeError('Lossless backup proof must be issued by the J2 lossless verifier')
    }
    const packageId = validatePackageId(request.backup.packageId)
    if (request.backup.libraryId !== request.plan.libraryId) {
        throw new TypeError('Lossless backup belongs to a different library')
    }
    if (
        !SHA256_PATTERN.test(request.backup.sourceManifestHash)
        || request.backup.sourceManifestHash !== requiredManifestHash
    ) {
        throw new TypeError('Lossless backup does not match the losing manifest')
    }
    validateComponents(request.backup.verifiedComponents)
    return {
        kind: 'authorized',
        winner: request.winner,
        losingSide,
        packageId,
        sourceManifestHash: requiredManifestHash,
    }
}
