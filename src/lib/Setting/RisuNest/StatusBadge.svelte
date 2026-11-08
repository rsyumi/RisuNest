<script lang="ts">
    interface Props {
        label: string
        /** `working` pulses its dot; `attention` and `connected` tint the badge. */
        tone: 'idle' | 'connected' | 'working' | 'paused' | 'failed' | 'attention'
    }

    let { label, tone }: Props = $props()
</script>

<span class="status" data-tone={tone} aria-live="polite"><span class="status-dot" aria-hidden="true"></span>{label}</span>

<style>
    .status {
        display: inline-flex;
        align-items: center;
        gap: 0.45rem;
        max-width: 100%;
        min-height: 1.75rem;
        padding: 0.25rem 0.7rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 99px;
        background: var(--risu-theme-bgcolor);
        color: var(--risu-theme-textcolor);
        font-size: 0.75rem;
        font-weight: 500;
        line-height: 1.3;
    }
    .status[data-tone='connected'] {
        border-color: color-mix(in srgb, var(--risu-theme-success-500) 45%, transparent);
        background: color-mix(in srgb, var(--risu-theme-success-500) 10%, var(--risu-theme-bgcolor));
    }
    .status[data-tone='working'] {
        border-color: color-mix(in srgb, var(--risu-theme-primary-500) 45%, transparent);
        background: color-mix(in srgb, var(--risu-theme-primary-500) 10%, var(--risu-theme-bgcolor));
    }
    .status[data-tone='attention'] {
        border-color: color-mix(in srgb, var(--risu-theme-danger-400) 55%, transparent);
        background: color-mix(in srgb, var(--risu-theme-danger-400) 10%, var(--risu-theme-bgcolor));
        color: color-mix(in srgb, var(--risu-theme-danger-400) 65%, var(--risu-theme-textcolor));
    }
    .status[data-tone='idle'] {
        color: var(--color-subtext);
    }
    .status-dot {
        flex: none;
        width: 0.45rem;
        height: 0.45rem;
        border-radius: 50%;
        background: currentColor;
        opacity: 0.45;
    }
    .status[data-tone='connected'] .status-dot {
        background: var(--risu-theme-success-500);
        opacity: 1;
    }
    .status[data-tone='attention'] .status-dot,
    .status[data-tone='failed'] .status-dot {
        opacity: 1;
    }
    .status[data-tone='paused'] .status-dot {
        background: transparent;
        box-shadow: inset 0 0 0 1.5px currentColor;
        opacity: 0.7;
    }
    .status[data-tone='working'] .status-dot {
        background: var(--risu-theme-primary-500);
        opacity: 1;
        animation: status-pulse 1.5s ease-in-out infinite;
    }
    @keyframes status-pulse {
        50% {
            opacity: 0.3;
        }
    }
    @media (prefers-reduced-motion: reduce) {
        .status-dot {
            animation: none;
        }
    }
</style>
