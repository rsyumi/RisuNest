<script lang="ts">
    import { onDestroy } from 'svelte'
    import SegmentedControl from 'src/lib/UI/GUI/SegmentedControl.svelte'
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
<SegmentedControl
    bind:value={profile}
    options={[
        { value: 'normal', label: language.risuNest.perf.profileNormal },
        { value: 'low-spec', label: language.risuNest.perf.profileLowSpec },
    ]}
/>
<span class="text-textcolor2 text-sm">{language.risuNest.perf.profileHelp}</span>
