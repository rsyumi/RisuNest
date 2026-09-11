<script lang="ts">
    import { onDestroy } from 'svelte'
    import { language } from 'src/lang'
    import { getDeviceSettings, subscribeDeviceSettings, updateDeviceSettings } from 'src/ts/storage/deviceSettings'
    import SettingGroup from '../RisuNest/SettingGroup.svelte'
    import SettingRow from '../RisuNest/SettingRow.svelte'
    import SegmentedButtons from '../RisuNest/SegmentedButtons.svelte'

    type Profile = ReturnType<typeof getDeviceSettings>['performanceProfile']

    let profile = $state(getDeviceSettings().performanceProfile)
    const unsubscribe = subscribeDeviceSettings((settings) => {
        profile = settings.performanceProfile
    })

    onDestroy(unsubscribe)

    const options: { value: Profile; label: string }[] = [
        { value: 'normal', label: language.risuNest.perf.profileNormal },
        { value: 'low-spec', label: language.risuNest.perf.profileLowSpec },
    ]

    function selectProfile(nextProfile: Profile) {
        if (nextProfile === profile) return
        profile = nextProfile
        updateDeviceSettings({
            performanceProfile: nextProfile,
        })
    }
</script>

<SettingGroup id="risunest-perf" title={language.risuNest.perf.title}>
    <SettingRow label={language.risuNest.perf.profile} help={language.risuNest.perf.profileHelp}>
        <SegmentedButtons value={profile} {options} label={language.risuNest.perf.profile} onchange={selectProfile} />
    </SettingRow>
</SettingGroup>
