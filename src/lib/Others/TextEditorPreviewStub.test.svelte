<script lang="ts">
    let { value, presetEditor = false }: { value: string; presetEditor?: boolean } = $props()
</script>

<div data-preview-stub data-preset-editor={presetEditor}>{value}</div>
