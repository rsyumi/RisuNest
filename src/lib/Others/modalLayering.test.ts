import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import { describe, expect, it } from 'vitest'

const alertComp = readFileSync('src/lib/Others/AlertComp.svelte', 'utf8')
const customSidebarConfig = readFileSync('src/lib/Others/CustomSidebarConfig.svelte', 'utf8')
const categoryManagerModal = readFileSync('src/lib/Others/HypaV3Modal/category-manager-modal.svelte', 'utf8')
const tagManagerModal = readFileSync('src/lib/Others/HypaV3Modal/tag-manager-modal.svelte', 'utf8')
const irisModal = readFileSync('src/lib/Others/IrisModal.svelte', 'utf8')
const nativeFileJobDialog = readFileSync('src/lib/Others/NativeFileJobDialog.svelte', 'utf8')
const popupEditor = readFileSync('src/lib/Others/PopupEditor.svelte', 'utf8')
const promptDiffModal = readFileSync('src/lib/Others/PromptDiffModal.svelte', 'utf8')
const easyPanel = readFileSync('src/lib/Others/ProTools/EasyPanel.svelte', 'utf8')
const defaultChatScreen = readFileSync('src/lib/ChatScreens/DefaultChatScreen.svelte', 'utf8')
const chatScreenshotDialog = readFileSync('src/lib/ChatScreens/ChatScreenshotDialog.svelte', 'utf8')
const botpreset = readFileSync('src/lib/Setting/botpreset.svelte', 'utf8')
const triggerV2List = readFileSync('src/lib/SideBars/Scripts/TriggerV2List.svelte', 'utf8')
const textAreaInput = readFileSync('src/lib/UI/GUI/TextAreaInput.svelte', 'utf8')
const modelList = readFileSync('src/lib/UI/ModelList.svelte', 'utf8')
const openrouterProviderList = readFileSync('src/lib/UI/OpenrouterProviderList.svelte', 'utf8')
const popupList = readFileSync('src/lib/UI/PopupList.svelte', 'utf8')
const promptDataItem = readFileSync('src/lib/UI/PromptDataItem.svelte', 'utf8')
const realmFrame = readFileSync('src/lib/UI/Realm/RealmFrame.svelte', 'utf8')
const realmMain = readFileSync('src/lib/UI/Realm/RealmMain.svelte', 'utf8')
const realmPopUp = readFileSync('src/lib/UI/Realm/RealmPopUp.svelte', 'utf8')
const realmUpload = readFileSync('src/lib/UI/Realm/RealmUpload.svelte', 'utf8')
const observer = readFileSync('src/ts/observer.svelte.ts', 'utf8')

/**
 * Full-screen modals (alerts, confirmations, permission prompts, pickers) sit on the shared
 * `z-modal` layer above the z-[1000] work dialogs. In-screen layers such as chat overlay
 * buttons, context menus, drag ghosts, and the text area stack keep their local z-index so
 * their internal ordering (for example the z-100 autocomplete above the z-50 editor) survives.
 */
const modalSources: Record<string, string> = {
    'AlertComp.svelte': alertComp,
    'PopupEditor.svelte': popupEditor,
    'IrisModal.svelte': irisModal,
    'CustomSidebarConfig.svelte': customSidebarConfig,
    'HypaV3Modal/category-manager-modal.svelte': categoryManagerModal,
    'HypaV3Modal/tag-manager-modal.svelte': tagManagerModal,
    'PromptDiffModal.svelte': promptDiffModal,
    'UI/ModelList.svelte': modelList,
    'UI/OpenrouterProviderList.svelte': openrouterProviderList,
    'UI/PopupList.svelte': popupList,
    'UI/Realm/RealmFrame.svelte': realmFrame,
    'UI/Realm/RealmMain.svelte': realmMain,
    'UI/Realm/RealmPopUp.svelte': realmPopUp,
    'UI/Realm/RealmUpload.svelte': realmUpload,
}

const inScreenSources: Record<string, string> = {
    'ChatScreens/DefaultChatScreen.svelte': defaultChatScreen,
    'SideBars/Scripts/TriggerV2List.svelte': triggerV2List,
    'Setting/botpreset.svelte': botpreset,
    'UI/PromptDataItem.svelte': promptDataItem,
    'UI/GUI/TextAreaInput.svelte': textAreaInput,
    'ProTools/EasyPanel.svelte': easyPanel,
    'ts/observer.svelte.ts': observer,
}

const workDialogSources: Record<string, string> = {
    'NativeFileJobDialog.svelte': nativeFileJobDialog,
    'ChatScreens/ChatScreenshotDialog.svelte': chatScreenshotDialog,
}

// Vitest serves CSS modules as empty strings, so read the stylesheet from disk.
const styles = readFileSync(join(process.cwd(), 'src', 'styles.css'), 'utf8')

function count(source: string, token: string): number {
    return source.split(token).length - 1
}

describe('modal layering', () => {
    it('defines the z-modal utility above the work dialog layer', () => {
        expect(styles).toMatch(/@utility z-modal \{\s*z-index: 2000;\s*\}/)
    })

    it('puts every full-screen modal on the z-modal layer', () => {
        for (const [name, source] of Object.entries(modalSources)) {
            expect(count(source, 'z-modal'), name).toBeGreaterThan(0)
            expect(/\bz-50\b/.test(source), `${name} still uses z-50`).toBe(false)
        }
        expect(count(alertComp, 'z-modal')).toBe(6)
    })

    it('leaves in-screen layers on their local z-index', () => {
        for (const [name, source] of Object.entries(inScreenSources)) {
            expect(count(source, 'z-modal'), name).toBe(0)
        }
        expect(count(textAreaInput, 'z-100')).toBe(1)
    })

    it('keeps work dialogs below the modal layer', () => {
        for (const [name, source] of Object.entries(workDialogSources)) {
            expect(count(source, 'z-[1000]'), name).toBe(1)
            expect(count(source, 'z-modal'), name).toBe(0)
        }
    })
})
