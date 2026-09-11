<script lang="ts">
    import type { SettingContext, SettingItem } from 'src/ts/setting/types'
    import { DBState } from 'src/ts/stores.svelte'
    import { getModelInfo } from 'src/ts/model/modellist'
    import { checkCondition, getLabel, resolveLanguagePath } from 'src/ts/setting/utils'
    import SettingGroup from './SettingGroup.svelte'
    import SettingRow from './SettingRow.svelte'
    import RisuNestSettingControl from './RisuNestSettingControl.svelte'

    interface Props {
        /** Setting items where every `header` item starts a new group. */
        items: SettingItem[]
    }

    let { items }: Props = $props()

    let ctx: SettingContext = $derived({
        db: DBState.db,
        modelInfo: getModelInfo(DBState.db.aiModel),
        subModelInfo: getModelInfo(DBState.db.subModel),
    })

    interface Group {
        header: SettingItem
        items: SettingItem[]
    }

    let groups = $derived.by(() => {
        const result: Group[] = []
        for (const item of items) {
            if (item.type === 'header') result.push({ header: item, items: [] })
            else result.at(-1)?.items.push(item)
        }
        return result
    })

    /** `risunest.streaming.header` becomes the anchor `risunest-streaming`. */
    function groupId(header: SettingItem): string {
        return header.id.replace(/\.header$/, '').replace(/\./g, '-')
    }

    function helpText(item: SettingItem): string | undefined {
        const value = item.helpKey ? resolveLanguagePath(item.helpKey) : undefined
        return typeof value === 'string' ? value : undefined
    }
</script>

{#each groups as group (group.header.id)}
    <SettingGroup id={groupId(group.header)} title={getLabel(group.header)}>
        {#each group.items as item (item.id)}
            {#if checkCondition(item, ctx)}
                <SettingRow label={getLabel(item)} help={helpText(item)} inline={item.type === 'check'}>
                    <RisuNestSettingControl {item} {ctx} />
                </SettingRow>
            {/if}
        {/each}
    </SettingGroup>
{/each}
