import { safeStructuredClone } from '../polyfill'
import type { Chat } from './database.svelte'

const metadataOnlySelectedConversation = Symbol('metadataOnlySelectedConversation')

export type MetadataOnlySelectedConversation = Chat & {
    readonly [metadataOnlySelectedConversation]: true
}

export function cloneConversationMetadata(
    conversation: Chat | Omit<Chat, 'message'>,
): Omit<Chat, 'message'> {
    const metadata = {} as Omit<Chat, 'message'>
    for (const key of Object.keys(conversation) as Array<keyof Chat>) {
        if (key === 'message') continue
        Object.defineProperty(metadata, key, {
            configurable: true,
            enumerable: true,
            value: safeStructuredClone(conversation[key]),
            writable: true,
        })
    }
    return metadata
}

export function createMetadataOnlySelectedConversation(
    conversation: Chat | Omit<Chat, 'message'>,
): MetadataOnlySelectedConversation {
    const shell = {} as MetadataOnlySelectedConversation
    const metadata = cloneConversationMetadata(conversation)
    for (const key of Object.keys(metadata) as Array<
        keyof Omit<Chat, 'message'>
    >) {
        Object.defineProperty(shell, key, {
            configurable: true,
            enumerable: true,
            value: metadata[key],
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
