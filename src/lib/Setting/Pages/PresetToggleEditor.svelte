<script lang="ts">
    import { presetEditorDBState as DBState } from 'src/ts/storage/presetEditor.svelte'
    import ToggleDefinitionEditor from 'src/lib/UI/ToggleDefinitionEditor.svelte'

    interface Props {
        className?: string
        hideLabel?: boolean
    }

    let { className = '', hideLabel = false }: Props = $props()
</script>
<ToggleDefinitionEditor {className} {hideLabel} value={DBState.db.customPromptTemplateToggle} onChange={(text) => {
    DBState.db.customPromptTemplateToggle = text
}} />
