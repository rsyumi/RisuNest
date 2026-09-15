import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import { describe, expect, it } from 'vitest'

import alertComp from './AlertComp.svelte?raw'
import customSidebarConfig from './CustomSidebarConfig.svelte?raw'
import categoryManagerModal from './HypaV3Modal/category-manager-modal.svelte?raw'
import tagManagerModal from './HypaV3Modal/tag-manager-modal.svelte?raw'
import irisModal from './IrisModal.svelte?raw'
import nativeFileJobDialog from './NativeFileJobDialog.svelte?raw'
import popupEditor from './PopupEditor.svelte?raw'
import promptDiffModal from './PromptDiffModal.svelte?raw'
import easyPanel from './ProTools/EasyPanel.svelte?raw'
import defaultChatScreen from '../ChatScreens/DefaultChatScreen.svelte?raw'
import chatScreenshotDialog from '../ChatScreens/ChatScreenshotDialog.svelte?raw'
import botpreset from '../Setting/botpreset.svelte?raw'
import triggerV2List from '../SideBars/Scripts/TriggerV2List.svelte?raw'
import textAreaInput from '../UI/GUI/TextAreaInput.svelte?raw'
import modelList from '../UI/ModelList.svelte?raw'
import openrouterProviderList from '../UI/OpenrouterProviderList.svelte?raw'
import popupList from '../UI/PopupList.svelte?raw'
import promptDataItem from '../UI/PromptDataItem.svelte?raw'
import realmFrame from '../UI/Realm/RealmFrame.svelte?raw'
import realmMain from '../UI/Realm/RealmMain.svelte?raw'
import realmPopUp from '../UI/Realm/RealmPopUp.svelte?raw'
import realmUpload from '../UI/Realm/RealmUpload.svelte?raw'
import observer from '../../ts/observer.svelte.ts?raw'

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
