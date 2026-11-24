import type { RisunestHostContext } from './risunestHostContext'

// Characters by ID: create, move to trash, restore.

export function createRisunestCharacterAccess(context: RisunestHostContext) {
    void context
    const notImplemented = (method: string) => Promise.reject(new Error(`${method} is not implemented`))
    return {
        createCharacter: (input?: unknown): Promise<unknown> => (void input, notImplemented('risunestCreateCharacter')),
        trashCharacter: (target?: unknown): Promise<unknown> => (void target, notImplemented('risunestTrashCharacter')),
        restoreCharacter: (target?: unknown): Promise<unknown> => (void target, notImplemented('risunestRestoreCharacter')),
    }
}
