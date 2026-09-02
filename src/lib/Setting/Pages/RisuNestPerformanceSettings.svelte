<script lang="ts">
    import { onDestroy } from 'svelte'
    import { language } from 'src/lang'
    import { getDeviceSettings, subscribeDeviceSettings, updateDeviceSettings } from 'src/ts/storage/deviceSettings'

    let profile = $state(getDeviceSettings().performanceProfile)
    const unsubscribe = subscribeDeviceSettings((settings) => {
        profile = settings.performanceProfile
    })

    onDestroy(unsubscribe)

    $effect(() => {
        updateDeviceSettings({
            performanceProfile: profile === 'low-spec' ? 'low-spec' : 'normal',
        })
    })
</script>

<h2 class="mb-2 text-2xl font-bold mt-2">{language.risuNest.perf.title}</h2>
<span class="text-textcolor">{language.risuNest.perf.profile}</span>
<div class="mb-4 inline-flex gap-0.5 rounded-lg border border-darkborderc bg-darkbg p-1" role="radiogroup" aria-label={language.risuNest.perf.profile}>
    {#each [
        { value: 'normal', label: language.risuNest.perf.profileNormal },
        { value: 'low-spec', label: language.risuNest.perf.profileLowSpec },
    ] as option}
        <button
            type="button"
            role="radio"
            aria-checked={profile === option.value}
            class="rounded-md px-4 py-2 text-sm {profile === option.value ? 'bg-darkborderc text-white' : 'text-textcolor2'}"
            onclick={() => { profile = option.value as typeof profile }}
        >{option.label}</button>
    {/each}
</div>
<span class="text-textcolor2 text-sm">{language.risuNest.perf.profileHelp}</span>
