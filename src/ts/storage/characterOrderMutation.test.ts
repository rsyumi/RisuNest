import { describe, expect, it } from 'vitest'
import type { PersistentRoot } from './persistentDataStore'
import {
    appendCharacterIdToOrder,
    removeCharacterIdFromOrder,
} from './characterOrderMutation'

function makeRoot(): PersistentRoot {
    return {
        characterOrder: [
            'unrelated-a',
            'target',
            {
                id: 'folder',
                name: 'Folder',
                color: '',
                data: ['unrelated-b', 'target', 'unrelated-b'],
            },
            'unrelated-a',
        ],
    } as PersistentRoot
}

describe('stable character order mutation', () => {
    it('removes only the target ID without normalizing unrelated entries', () => {
        const root = makeRoot()

        removeCharacterIdFromOrder(root, 'target')

        expect(root.characterOrder).toEqual([
            'unrelated-a',
            {
                id: 'folder',
                name: 'Folder',
                color: '',
                data: ['unrelated-b', 'unrelated-b'],
            },
            'unrelated-a',
        ])
    })

    it('appends a missing target once and preserves an existing placement', () => {
        const missing = makeRoot()
        removeCharacterIdFromOrder(missing, 'target')

        appendCharacterIdToOrder(missing, 'target')
        appendCharacterIdToOrder(missing, 'target')

        expect(missing.characterOrder.at(-1)).toBe('target')
        expect(JSON.stringify(missing.characterOrder).match(/target/g)).toHaveLength(1)
    })
})
