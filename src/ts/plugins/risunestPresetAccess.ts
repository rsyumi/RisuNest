import type { RisunestHostContext } from './risunestHostContext'

// Model presets by ID: list, read, update, create, delete, select.

export function createRisunestPresetAccess(context: RisunestHostContext) {
    void context
    const notImplemented = (method: string) => Promise.reject(new Error(`${method} is not implemented`))
    return {
        listPresets: (): Promise<unknown> => notImplemented('risunestListPresets'),
        readPreset: (target?: unknown, options?: unknown): Promise<unknown> => (void target, void options, notImplemented('risunestReadPreset')),
        updatePreset: (target?: unknown, set?: unknown): Promise<unknown> => (void target, void set, notImplemented('risunestUpdatePreset')),
        createPreset: (input?: unknown): Promise<unknown> => (void input, notImplemented('risunestCreatePreset')),
        deletePreset: (target?: unknown): Promise<unknown> => (void target, notImplemented('risunestDeletePreset')),
        selectPreset: (target?: unknown): Promise<unknown> => (void target, notImplemented('risunestSelectPreset')),
    }
}
