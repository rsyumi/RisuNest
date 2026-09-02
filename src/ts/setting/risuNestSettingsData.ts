import type { SettingItem } from './types'
import { MAX_INLAY_DIMENSION } from '../storage/blobStore'

export const risuNestSettingsItems: SettingItem[] = [
    { id: 'risunest.inlay.header', type: 'header', labelKey: 'risuNest.inlay.title', options: { level: 'h2' } },
    {
        id: 'risunest.inlay.format',
        type: 'select',
        labelKey: 'risuNest.inlay.format',
        helpKey: 'risuNest.inlay.formatHelp',
        bindKey: 'risunestInlayFormat',
        options: {
            selectOptions: [
                { value: 'webp', labelKey: 'risuNest.inlay.formatWebp' },
                { value: 'png', labelKey: 'risuNest.inlay.formatPng' },
                { value: 'original', labelKey: 'risuNest.inlay.formatOriginal' },
            ],
        },
    },
    {
        id: 'risunest.inlay.quality',
        type: 'slider',
        labelKey: 'risuNest.inlay.quality',
        helpKey: 'risuNest.inlay.qualityHelp',
        bindKey: 'risunestInlayWebpQuality',
        condition: (ctx) => ctx.db.risunestInlayFormat === 'webp',
        options: { min: 1, max: 100, step: 1 },
    },
    {
        id: 'risunest.inlay.maxDimension',
        type: 'number',
        labelKey: 'risuNest.inlay.maxDimension',
        helpKey: 'risuNest.inlay.maxDimensionHelp',
        bindKey: 'risunestInlayMaxDimension',
        condition: (ctx) => ctx.db.risunestInlayFormat !== 'original',
        options: { min: 0, max: MAX_INLAY_DIMENSION, step: 1 },
    },
    {
        id: 'risunest.inlay.skip',
        type: 'check',
        labelKey: 'risuNest.inlay.skipReencode',
        helpKey: 'risuNest.inlay.skipReencodeHelp',
        bindKey: 'risunestInlaySkipReencode',
        condition: (ctx) => ctx.db.risunestInlayFormat === 'webp',
    },
]
