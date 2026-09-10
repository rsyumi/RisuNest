import type { SettingItem } from './types'
import { MAX_INLAY_DIMENSION, normalizeInlayEncodeOptions } from '../storage/blobStore'

/** Unit shown beside a numeric control, keyed by setting id. */
export const risuNestSettingUnits: Record<string, string> = {
    'risunest.inlay.maxDimension': 'px',
}

export const risuNestSettingsItems: SettingItem[] = [
    {
        id: 'risunest.streaming.header',
        type: 'header',
        labelKey: 'risuNest.streaming.title',
        options: { level: 'h2' },
    },
    {
        id: 'risunest.streaming.thoughtMode',
        type: 'segmented',
        labelKey: 'risuNest.streaming.thoughtMode',
        helpKey: 'risuNest.streaming.thoughtModeHelp',
        bindKey: 'streamingThoughtMode',
        options: {
            segmentOptions: [
                { value: 'recent', labelKey: 'risuNest.streaming.recent' },
                {
                    value: 'collapsed',
                    labelKey: 'risuNest.streaming.collapsed',
                },
                { value: 'off', labelKey: 'risuNest.streaming.off' },
            ],
        },
    },
    {
        id: 'risunest.streaming.deferEffects',
        type: 'check',
        labelKey: 'risuNest.streaming.deferEffects',
        helpKey: 'risuNest.streaming.deferEffectsHelp',
        bindKey: 'streamingDeferDisplayProcessing',
    },
    { id: 'risunest.inlay.header', type: 'header', labelKey: 'risuNest.inlay.title', options: { level: 'h2' } },
    {
        id: 'risunest.inlay.format',
        type: 'select',
        labelKey: 'risuNest.inlay.format',
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
        setValue: (db, value: number) => {
            db.risunestInlayMaxDimension = normalizeInlayEncodeOptions({ maxDimension: value }).maxDimension
        },
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
