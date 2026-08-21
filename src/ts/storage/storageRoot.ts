export type BlobStorageRoot = { kind: 'legacy' } | { kind: 'generation'; id: string }

const GENERATED_STORAGE_ROOT_ID = /^[A-Za-z0-9_-]{1,64}$/

export function isGeneratedStorageRootId(id: string): boolean {
    return GENERATED_STORAGE_ROOT_ID.test(id)
}

export function assertGeneratedStorageRootId(id: string): void {
    if (!isGeneratedStorageRootId(id)) {
        throw new TypeError('Invalid storage generation identifier')
    }
}
