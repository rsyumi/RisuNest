<option value={value} selected={selected} disabled={disabled} class="bg-darkbg appearance-none">{@render children?.()}</option>
<script lang="ts">
    interface Props {
        value: string | number;
        selected?: boolean;
        disabled?: boolean;
        children?: import('svelte').Snippet;
    }

    let { value, selected = false, disabled = false, children }: Props = $props();
</script>