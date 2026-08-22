export interface ColdPayloadStore {
    read(key: string): Promise<Uint8Array | null>
    write(key: string, data: Uint8Array): Promise<void>
    list(): Promise<string[]>
    remove(key: string): Promise<void>
}
