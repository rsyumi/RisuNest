import type { SettingItem, SettingContext } from './types';
import { DBState } from '../stores.svelte';
import { language } from 'src/lang';
import { accessibilitySettingsItems } from './accessibilitySettingsData';
import { advancedSettingsItems } from './advancedSettingsData';
import { basicParameterItems, modelSpecificParameterItems, penaltyParameterItems, samplingParameterItems, seedSetting } from './botSettingsParamsData';
import { chatFormatSettingsItems } from './chatFormatSettingsData';
import { displaySettingsItems } from './displaySettingsData.svelte';
import { risuNestSettingsItems } from './risuNestSettingsData';

/**
 * Sentinel value representing an uninitialized local state in wrapper components.
 * Used instead of `undefined` so that a legitimate `undefined` DB value
 * can still be written back without being silently ignored.
 */
export const UNINITIALIZED = Symbol('uninitialized');

export function resolveLanguagePath(path: string): unknown {
    let value: unknown = language;
    for (const segment of path.split('.')) {
        if (!value || typeof value !== 'object') return undefined;
        value = (value as Record<string, unknown>)[segment];
    }
    return value;
}

export function getLabel(item: SettingItem): string {
    const label = item.labelKey ? resolveLanguagePath(item.labelKey) : undefined;
    if (typeof label === 'string') {
        return label;
    }
    return item.fallbackLabel ?? '';
}

// The context names the database its page edits; the settings window edits the selected preset.
export function getSettingValue(item: SettingItem, ctx: SettingContext): any {
    const db = ctx.db ?? DBState.db;
    if (item.getValue) {
        return item.getValue(db, ctx);
    }
    if (item.bindPath) {
        const parts = item.bindPath.split('.');
        let value: any = db;
        for (const part of parts) {
            value = value?.[part];
        }
        return value;
    }
    if (item.bindKey) {
        return (db as any)[item.bindKey];
    }
    return undefined;
}

export function setSettingValue(item: SettingItem, newValue: any, ctx: SettingContext): void {
    const db = ctx.db ?? DBState.db;
    if (item.setValue) {
        item.setValue(db, newValue, ctx);
    } else if (item.bindPath) {
        const parts = item.bindPath.split('.');
        let obj: any = db;
        for (let i = 0; i < parts.length - 1; i++) {
            obj = obj[parts[i]] ??= {};
        }
        obj[parts[parts.length - 1]] = newValue;
    } else if (item.bindKey) {
        (db as any)[item.bindKey] = newValue;
    }
    
    if (item.onChange) {
        item.onChange(newValue, ctx);
    }
}

/**
 * Check if item should be visible based on condition
 */
export function checkCondition(item: SettingItem, ctx: SettingContext): boolean {
    if (!item.condition) return true;
    return item.condition(ctx);
}

export function getFullSettingsData(searchTerm = '') {
    const full =  accessibilitySettingsItems.concat(
        advancedSettingsItems,
        basicParameterItems,
        seedSetting,
        samplingParameterItems,
        penaltyParameterItems,
        modelSpecificParameterItems,
        chatFormatSettingsItems,
        displaySettingsItems,
        risuNestSettingsItems,
    );

    if(!searchTerm) return full;

    const lowerSearch = searchTerm.toLowerCase();
    return full.filter(item => {
        const label = getLabel(item).toLowerCase();
        const keywords = item.keywords?.map(k => k.toLowerCase()) || [];
        return label.includes(lowerSearch) || keywords.some(k => k.includes(lowerSearch));
    });


}
