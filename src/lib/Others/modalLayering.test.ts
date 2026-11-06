import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import { describe, expect, it } from 'vitest'

const alertComp = readFileSync('src/lib/Others/AlertComp.svelte', 'utf8')
const customSidebarConfig = readFileSync('src/lib/Others/CustomSidebarConfig.svelte', 'utf8')
const categoryManagerModal = readFileSync('src/lib/Others/HypaV3Modal/category-manager-modal.svelte', 'utf8')
const tagManagerModal = readFileSync('src/lib/Others/HypaV3Modal/tag-manager-modal.svelte', 'utf8')
const irisModal = readFileSync('src/lib/Others/IrisModal.svelte', 'utf8')
const onboardingModal = readFileSync('src/lib/Others/OnboardingModal.svelte', 'utf8')
const nativeFileJobDialog = readFileSync('src/lib/Others/NativeFileJobDialog.svelte', 'utf8')
const persistentWorkingSetRecovery = readFileSync('src/lib/Others/PersistentWorkingSetRecovery.svelte', 'utf8')
const updatePopup = readFileSync('src/lib/Others/UpdatePopup.svelte', 'utf8')
const popupEditor = readFileSync('src/lib/Others/PopupEditor.svelte', 'utf8')
const textEditorPopup = readFileSync('src/lib/Others/TextEditorPopup.svelte', 'utf8')
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
 * `z-modal` layer above the z-[1000] work dialogs. The two work dialogs that have to cover
 * the z-[1000] ones name their layer instead of writing it out: `z-work-dialog-update` for
 * the update dialog and `z-work-dialog-recovery` for the working-set recovery. In-screen
 * layers such as chat overlay buttons, context menus, drag ghosts, and the text area stack
 * keep their local z-index so their internal ordering (for example the z-100 autocomplete
 * above the z-50 editor) survives. The code block menu is appended to the body and opens over
 * whichever screen holds the code, so it sits on `z-modal-menu` above the modals.
 */
const modalSources: Record<string, string> = {
    'AlertComp.svelte': alertComp,
    'PopupEditor.svelte': popupEditor,
    'TextEditorPopup.svelte': textEditorPopup,
    'IrisModal.svelte': irisModal,
    'OnboardingModal.svelte': onboardingModal,
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
}

const workDialogSources: Record<string, string> = {
    'NativeFileJobDialog.svelte': nativeFileJobDialog,
    'ChatScreens/ChatScreenshotDialog.svelte': chatScreenshotDialog,
}

const coveringDialogSources: Record<string, string> = {
    'UpdatePopup.svelte': updatePopup,
    'PersistentWorkingSetRecovery.svelte': persistentWorkingSetRecovery,
}

// Vitest serves CSS modules as empty strings, so read the stylesheet from disk.
const styles = readFileSync(join(process.cwd(), 'src', 'styles.css'), 'utf8')

function count(source: string, token: string): number {
    return source.split(token).length - 1
}

function utilityZIndex(name: string): number {
    const match = styles.match(new RegExp(`@utility ${name} \\{\\s*z-index: (\\d+);\\s*\\}`))
    expect(match, name).not.toBeNull()
    return Number(match![1])
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
        expect(count(alertComp, 'z-modal')).toBe(7)
        const checkboxDialog = alertComp.slice(
            alertComp.indexOf("{#if $alertStore.type === 'checkboxConfirm'"),
            alertComp.indexOf("{:else if $alertStore.type !== 'none'"),
        )
        expect(count(checkboxDialog, 'z-modal')).toBe(1)
        expect(checkboxDialog).toContain('role="dialog"')
        expect(checkboxDialog).toContain('aria-modal="true"')
        expect(checkboxDialog).toContain('aria-labelledby="checkbox-confirm-title"')
        expect(checkboxDialog).toContain('aria-describedby="checkbox-confirm-description"')
    })

    it('mounts the text editor popup and the onboarding before the alerts so alerts cover them', () => {
        const app = readFileSync('src/App.svelte', 'utf8')
        for (const mount of ['<TextEditorPopup ', '<OnboardingModal />']) {
            expect(app.indexOf(mount), mount).toBeGreaterThan(-1)
            expect(app.indexOf(mount), mount).toBeLessThan(app.indexOf('<AlertComp />'))
        }
    })

    it('leaves in-screen layers on their local z-index', () => {
        for (const [name, source] of Object.entries(inScreenSources)) {
            expect(count(source, 'z-modal'), name).toBe(0)
        }
        expect(count(textAreaInput, 'z-100')).toBe(1)
    })

    it('opens the code block menu above the modals and below the app lock', () => {
        const menuLayer = utilityZIndex('z-modal-menu')
        expect(menuLayer).toBeGreaterThan(utilityZIndex('z-modal'))
        expect(menuLayer).toBeLessThan(utilityZIndex('z-app-lock'))
        expect(count(observer, 'z-modal-menu')).toBe(1)
        expect(/\bz-50\b/.test(observer)).toBe(false)
    })

    it('keeps work dialogs below the modal layer', () => {
        for (const [name, source] of Object.entries(workDialogSources)) {
            expect(count(source, 'z-[1000]'), name).toBe(1)
            expect(count(source, 'z-modal'), name).toBe(0)
        }
    })

    it('names the layers of the work dialogs that cover the others', () => {
        expect(styles).toMatch(/@utility z-work-dialog-update \{\s*z-index: 1001;\s*\}/)
        expect(styles).toMatch(/@utility z-work-dialog-recovery \{\s*z-index: 1100;\s*\}/)
        expect(count(updatePopup, 'z-work-dialog-update')).toBe(1)
        expect(count(persistentWorkingSetRecovery, 'z-work-dialog-recovery')).toBe(1)
        for (const [name, source] of Object.entries(coveringDialogSources)) {
            expect(/z-\[\d+\]/.test(source), `${name} still writes its z-index out`).toBe(false)
            expect(count(source, 'z-modal'), name).toBe(0)
        }
    })
})
