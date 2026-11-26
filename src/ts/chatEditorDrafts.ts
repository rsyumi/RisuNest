import isEqual from 'lodash/isEqual'
import type { SelectedConversationMessageEditIntent } from './selectedConversationOperations'
import type { Message } from './storage/database.svelte'
import { closeTextEditorPopup, type TextEditorPopupRequest } from './gui/textEditorPopup.svelte'

export interface ChatEditorPopupHandlers {
    save(value: string): Promise<boolean>
    input(value: string): void
    cancel(): void
}

/** The open popup calls through `owner`, so a remounted row can take it over. */
export interface ChatEditorPopup {
    request: TextEditorPopupRequest
    owner: ChatEditorPopupHandlers
}

/** An open message editor of a row that was torn down, kept until the same message mounts again. */
export interface ChatEditorDraft {
    kind: 'original' | 'translation'
    draft: string
    /** Absolute message index the editor was showing. */
    index: number
    /** The message as it was when the edit began. */
    evidence: Readonly<Message>
    caret?: readonly [number, number]
    popup?: ChatEditorPopup
    translationKey?: string
    intent?: SelectedConversationMessageEditIntent
}

const drafts = new Map<string, ChatEditorDraft[]>()

function conversationKey(characterId: string, conversationId: string): string {
    return `${characterId.length}:${characterId}|${conversationId}`
}

export function keepEditorDraft(characterId: string, conversationId: string, draft: ChatEditorDraft): void {
    const key = conversationKey(characterId, conversationId)
    const editor = draft.intent?.windowedEditor
    if (editor) draft.intent = { ...draft.intent!, windowedEditor: editor.retain() }
    drafts.set(key, [...(drafts.get(key) ?? []), draft])
}

export function pendingEditorDrafts(characterId: string, conversationId: string): readonly ChatEditorDraft[] {
    return drafts.get(conversationKey(characterId, conversationId)) ?? []
}

export function editorDraftMatches(draft: ChatEditorDraft, index: number, message: Readonly<Message>): boolean {
    const editor = draft.intent?.windowedEditor
    return editor ? editor.location() === index : draft.index === index && isEqual(message, draft.evidence)
}

/** Removes and returns the draft whose message is exactly the one now at `index`. */
export function takeEditorDraft(
    characterId: string,
    conversationId: string,
    index: number,
    message: Readonly<Message>,
): ChatEditorDraft | null {
    const key = conversationKey(characterId, conversationId)
    const list = drafts.get(key)
    const draft = list?.find((value) => editorDraftMatches(value, index, message))
    if (!list || !draft) return null
    remove(key, list, draft)
    return draft
}

function remove(key: string, list: ChatEditorDraft[], draft: ChatEditorDraft): void {
    const next = list.filter((value) => value !== draft)
    if (next.length) drafts.set(key, next)
    else drafts.delete(key)
}

/** Drops the kept draft whose popup was saved or cancelled by any row. */
export function forgetEditorPopup(popup: ChatEditorPopup | null): void {
    if (!popup) return
    for (const [key, list] of drafts) {
        const draft = list.find((value) => value.popup === popup)
        if (draft) { remove(key, list, draft); draft.intent?.windowedEditor?.release() }
    }
}

export function discardEditorDraft(characterId: string, conversationId: string, draft: ChatEditorDraft): void {
    const key = conversationKey(characterId, conversationId)
    const list = drafts.get(key)
    if (list?.includes(draft)) { remove(key, list, draft); draft.intent?.windowedEditor?.release() }
    if (draft.popup) closeTextEditorPopup(draft.popup.request)
}

/** Discards every draft that does not belong to the given conversation. */
export function discardEditorDraftsExcept(characterId: string | null, conversationId: string | null): void {
    const kept = characterId !== null && conversationId !== null ? conversationKey(characterId, conversationId) : null
    for (const [key, list] of [...drafts]) {
        if (key === kept) continue
        drafts.delete(key)
        for (const draft of list) {
            draft.intent?.windowedEditor?.release()
            if (draft.popup) closeTextEditorPopup(draft.popup.request)
        }
    }
}
