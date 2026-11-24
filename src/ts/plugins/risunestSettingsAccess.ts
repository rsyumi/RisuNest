import type { RisunestHostContext } from './risunestHostContext'

// Settings by key: risunestListSettingKeys, risunestReadSettings, risunestWriteSettings.

export function createRisunestSettingsAccess(context: RisunestHostContext) {
    void context
    const notImplemented = (method: string) => Promise.reject(new Error(`${method} is not implemented`))
    return {
        listSettingKeys: (): Promise<unknown> => notImplemented('risunestListSettingKeys'),
        readSettings: (keys?: unknown): Promise<unknown> => (void keys, notImplemented('risunestReadSettings')),
        writeSettings: (values?: unknown): Promise<unknown> => (void values, notImplemented('risunestWriteSettings')),
    }
}
