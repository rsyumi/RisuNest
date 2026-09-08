<script lang="ts">
    let {
        label,
        detail,
        elapsedText,
        compact = false,
    }: { label: string; detail?: string; elapsedText?: string; compact?: boolean } = $props()
</script>

<div class="loading-progress">
    <div class="loading-indicator" class:compact role="status" aria-live="polite">
        <span class="loading-ring" aria-hidden="true"></span>
        <span class="loading-copy">
            <span>{label}</span>
            {#if detail}
                <span class="loading-detail">{detail}</span>
            {/if}
        </span>
    </div>
    {#if elapsedText}
        <span class="loading-detail" aria-live="off">{elapsedText}</span>
    {/if}
</div>

<style>
    .loading-progress {
        display: inline-flex;
        flex-direction: column;
        align-items: center;
        gap: 0.25rem;
    }

    .loading-indicator {
        display: flex;
        flex-direction: column;
        align-items: center;
        justify-content: center;
        gap: 0.75rem;
        text-align: center;
    }

    .loading-indicator.compact {
        display: inline-flex;
        flex-direction: row;
        gap: 0.5rem;
        text-align: start;
    }

    .loading-ring {
        width: 1.25rem;
        height: 1.25rem;
        flex: none;
        border: 2px solid var(--risu-theme-borderc);
        border-top-color: var(--risu-theme-textcolor2);
        border-radius: 9999px;
        animation: loading-ring-spin 0.8s linear infinite;
    }

    .loading-copy {
        display: flex;
        flex-direction: column;
        align-items: center;
        gap: 0.25rem;
    }

    .compact .loading-copy {
        align-items: flex-start;
    }

    .loading-detail {
        color: var(--risu-theme-textcolor2);
        font-size: 0.875rem;
    }

    @keyframes loading-ring-spin {
        to {
            transform: rotate(360deg);
        }
    }

    @media (prefers-reduced-motion: reduce) {
        .loading-ring {
            animation: none;
        }
    }
</style>
