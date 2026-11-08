<script lang="ts">
    import { InfoIcon, TriangleAlertIcon } from '@lucide/svelte'

    interface Props {
        /** The whole message; it is the element's only text so it can be read back exactly. */
        text: string
        /** `danger` reports a failure or a risk, `info` a neutral state the reader should know. */
        tone?: 'danger' | 'info'
        role?: 'alert' | 'status'
    }

    let { text, tone = 'danger', role }: Props = $props()
</script>

<div class="notice" data-notice={tone} {role}>{#if tone === 'danger'}<TriangleAlertIcon size={16} class="notice-icon" aria-hidden="true" />{:else}<InfoIcon size={16} class="notice-icon" aria-hidden="true" />{/if}<p>{text}</p></div>

<style>
    .notice {
        display: flex;
        align-items: flex-start;
        gap: 0.55rem;
        min-width: 0;
        padding: 0.55rem 0.75rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
        color: var(--risu-theme-textcolor);
        font-size: 13px;
        line-height: 1.5;
        overflow-wrap: anywhere;
        white-space: pre-line;
    }
    .notice p {
        margin: 0;
        min-width: 0;
    }
    .notice[data-notice='danger'] {
        border-color: color-mix(in srgb, var(--risu-theme-danger-400) 40%, transparent);
        background: color-mix(in srgb, var(--risu-theme-danger-400) 8%, var(--risu-theme-bgcolor));
    }
    .notice :global(.notice-icon) {
        flex: none;
        margin-top: 0.15rem;
        color: var(--color-subtext);
    }
    .notice[data-notice='danger'] :global(.notice-icon) {
        color: color-mix(in srgb, var(--risu-theme-danger-400) 80%, var(--risu-theme-textcolor));
    }
</style>
