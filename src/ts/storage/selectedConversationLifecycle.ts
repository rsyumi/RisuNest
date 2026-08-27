import { safeStructuredClone } from '../polyfill'
import type { Chat } from './database.svelte'

const metadataOnlySelectedConversation = Symbol('metadataOnlySelectedConversation')

export type MetadataOnlySelectedConversation = Chat & {
    readonly [metadataOnlySelectedConversation]: true
}

export function createMetadataOnlySelectedConversation(
    conversation: Chat,
): MetadataOnlySelectedConversation {
    const shell = {} as MetadataOnlySelectedConversation
    for (const key of Object.keys(conversation) as Array<keyof Chat>) {
        if (key === 'message') continue
        Object.defineProperty(shell, key, {
            configurable: true,
            enumerable: true,
            value: safeStructuredClone(conversation[key]),
            writable: true,
        })
    }
    Object.defineProperty(shell, 'message', {
        configurable: false,
        enumerable: false,
        get(): never {
            throw new Error('Selected conversation is metadata-only')
        },
    })
    Object.defineProperty(shell, metadataOnlySelectedConversation, {
        configurable: false,
        enumerable: false,
        value: true,
        writable: false,
    })
    return shell
}

export function isMetadataOnlySelectedConversation(
    conversation: Chat,
): conversation is MetadataOnlySelectedConversation {
    return metadataOnlySelectedConversation in conversation
}
