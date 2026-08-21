import { compressSync } from 'fflate'
import type { LosslessMigrationInputEntry } from '../losslessMigrationPackage'

const encoder = new TextEncoder()

export const migrationFixtureReferences = {
    assets: ['assets/photo.png', 'assets/photo.jpg', 'assets/voice.mp3', 'assets/movie.webm'],
    inlays: ['image-inlay', 'audio-inlay', 'video-inlay', 'signature-inlay'],
    cold: ['character-cold', 'chat-cold'],
} as const

export function makeLosslessMigrationFixture(): LosslessMigrationInputEntry[] {
    const coldCharacter = compressSync(encoder.encode(JSON.stringify({ character: { chaId: 'cold-character' } })))
    const coldChat = compressSync(encoder.encode(JSON.stringify([{ role: 'char', data: 'cold message' }])))
    return [
        {
            kind: 'database', id: 'database.risudat', metadata: {},
            data: encoder.encode(JSON.stringify({ characters: [], marker: 'new' })),
        },
        {
            kind: 'asset', id: 'assets/photo.png',
            metadata: { kind: 'asset', mime: 'image/png', name: 'photo.png', ext: 'png' },
            data: new Uint8Array([1, 2, 3]),
        },
        {
            kind: 'asset', id: 'assets/photo.jpg',
            metadata: { kind: 'asset', mime: 'image/jpeg', name: 'photo.jpg', ext: 'jpg' },
            data: new Uint8Array([4, 5]),
        },
        {
            kind: 'asset', id: 'assets/voice.mp3',
            metadata: { kind: 'asset', mime: 'audio/mpeg', name: 'voice.mp3', ext: 'mp3' },
            data: new Uint8Array([6, 7]),
        },
        {
            kind: 'asset', id: 'assets/movie.webm',
            metadata: { kind: 'asset', mime: 'video/webm', name: 'movie.webm', ext: 'webm' },
            data: new Uint8Array([8, 9]),
        },
        {
            kind: 'inlay', id: 'image-inlay',
            metadata: { kind: 'inlay', mime: 'image/png', name: 'image', ext: 'png', inlayType: 'image', width: 32, height: 16 },
            data: new Uint8Array([10]),
        },
        {
            kind: 'inlay', id: 'audio-inlay',
            metadata: { kind: 'inlay', mime: 'audio/mpeg', name: 'audio', ext: 'mp3', inlayType: 'audio' },
            data: new Uint8Array([11]),
        },
        {
            kind: 'inlay', id: 'video-inlay',
            metadata: { kind: 'inlay', mime: 'video/webm', name: 'video', ext: 'webm', inlayType: 'video' },
            data: new Uint8Array([12]),
        },
        {
            kind: 'inlay', id: 'signature-inlay',
            metadata: { kind: 'inlay', mime: 'application/json', name: 'signature', ext: 'json', inlayType: 'signature' },
            data: encoder.encode('{"signature":true}'),
        },
        { kind: 'cold', id: 'character-cold', metadata: {}, data: coldCharacter },
        { kind: 'cold', id: 'chat-cold', metadata: {}, data: coldChat },
    ]
}
