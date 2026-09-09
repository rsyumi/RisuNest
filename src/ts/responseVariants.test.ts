import { expect, it } from 'vitest'
import { createPinnedBackupReferenceAccumulator } from './drive/backupAssets'
import { captureResponseVariants, projectResponseVariant, responseEditReplacement } from './responseVariants'
import type { Message } from './storage/database.svelte'

it('keeps group member edits and generation metadata with their selected candidate', () => {
    let id = 0
    const messages: Message[] = [
        { role: 'char', saying: 'a', data: 'A', chatId: 'a' },
        { role: 'char', saying: 'b', data: 'B', chatId: 'b', time: 42 },
    ]
    const variants = captureResponseVariants(messages, () => `${++id}`)
    variants.candidates.push({ id: 'other', messages: [{ role: 'char', data: 'Other', time: 12 }] })
    const projected = projectResponseVariant(variants, variants.selectedId)
    const edited = responseEditReplacement(projected, 0, { ...projected[0], data: 'Edited', time: 84 })
    const saved = edited.at(-1)!.responseVariants!
    expect(
        projectResponseVariant(saved, variants.selectedId).map((message) => [message.data, message.time]),
    ).toEqual([
        ['Edited', 84],
        ['B', 42],
    ])
    expect(projectResponseVariant(saved, 'other')[0]).toMatchObject({ data: 'Other', time: 12, chatId: 'b' })
})

it('keeps inactive candidate and pending recovery assets alive in backup reference collection', () => {
    const accumulator = createPinnedBackupReferenceAccumulator('full')
    accumulator.visitConversation({
        summary: { id: 'chat', characterId: 'character' },
        value: {
            id: 'chat',
            message: [
                {
                    role: 'char',
                    data: 'selected',
                    responseVariants: {
                        groupId: 'response',
                        selectedId: 'selected',
                        candidates: [
                            { id: 'selected', messages: [{ role: 'char', data: 'selected' }] },
                            {
                                id: 'inactive',
                                messages: [
                                    {
                                        role: 'char',
                                        data: '{{inlay::inactive}}',
                                        image: 'assets/inactive.png',
                                    },
                                ],
                            },
                        ],
                    },
                },
            ],
            rerollRecovery: {
                original: [{ role: 'char', data: '{{inlay::original}}', image: 'assets/original.png' }],
            },
        },
    } as never)
    expect(accumulator.finish().inlayKeys).toEqual(['inactive', 'original'])
    expect(accumulator.finish().assetKeys).toEqual(['assets/inactive.png', 'assets/original.png'])
})
