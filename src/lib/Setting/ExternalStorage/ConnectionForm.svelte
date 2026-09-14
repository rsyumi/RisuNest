<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import Button from 'src/lib/UI/GUI/Button.svelte'
    import TextInput from 'src/lib/UI/GUI/TextInput.svelte'
    import SelectInput from 'src/lib/UI/GUI/SelectInput.svelte'
    import { openUrl } from '@tauri-apps/plugin-opener'
    import { type as osType } from '@tauri-apps/plugin-os'
    import { isTauriAndroid, isTauriIOS } from 'src/ts/platform'
    import { getExternalStorageBridge } from 'src/ts/storage/sync/external/bridge'
    import {
        BACKUP_ONLY_ACKNOWLEDGEMENT,
        GITHUB_DEDICATED_REPOSITORY_ACKNOWLEDGEMENT,
        SEQUENTIAL_ACKNOWLEDGEMENT,
        buildPrepareConnectionRequest,
        defaultExternalStorageScope,
        requiredConnectionAcknowledgements,
    } from 'src/ts/storage/sync/external/connection'
    import {
        buildProviderSecret,
        externalProviderDefinitions,
        getExternalProviderDefinition,
    } from 'src/ts/storage/sync/external/providerRegistry'
    import type {
        ExternalConnectionResult,
        ExternalConnectionPurpose,
        ExternalOpenMode,
        ExternalProviderId,
        ExternalProviderDescriptor,
        ExternalPublicationStrategy,
        PreparedExternalConnection,
    } from 'src/ts/storage/sync/external/types'
    import type { ExternalStorageStrings } from './strings'

    interface Props {
        strings: ExternalStorageStrings
        onconnected: (result: ExternalConnectionResult) => void | Promise<void>
        oncancel: () => void
        onbusychange?: (busy: boolean) => void
    }

    let { strings, onconnected, oncancel, onbusychange = () => {} }: Props = $props()
    const bridge = getExternalStorageBridge()
    const platform = isTauriAndroid
        ? 'android'
        : isTauriIOS
          ? 'ios'
          : ({ windows: 'windows', macos: 'macos', linux: 'linux' } as Record<string, string>)[osType()] ?? 'windows'
    let providerId = $state<ExternalProviderId>('google_drive')
    let mode = $state<ExternalOpenMode>('create')
    let purpose = $state<ExternalConnectionPurpose>('backup')
    let strategy = $state<ExternalPublicationStrategy>('backup-only')
    let values = $state<Record<string, string>>({
        space: 'drive', accountType: 'personal', tenant: 'common',
        ...(isTauriAndroid ? {
            oauthRedirectUri: 'https://update.rsyumi.workers.dev/oauth/google-drive-callback.html',
        } : {}),
    })
    let deviceSettings = $state(true)
    let devicePlugins = $state(false)
    let accepted = $state<string[]>([])
    let prepared = $state<PreparedExternalConnection | null>(null)
    let endpointConfirmed = $state(false)
    let recoveryPayload = $state('')
    let recoveryCode = $state('')
    let pendingAuthorizationId = $state<string | null>(null)
    let currentPlatformClientId = $state('')
    let oauthClientSecret = $state('')
    let manualOAuthCallback = $state('')
    let providerDescriptors = $state<ExternalProviderDescriptor[]>([])
    let busy = $state(false)
    let error = $state('')
    let authorizationStatus = $state('')
    let destroyed = false
    let authorizationCompletionInFlight = false
    let cancellationId: string | null = null
    let cancellationPromise: Promise<boolean> | null = null

    const definition = $derived(getExternalProviderDefinition(providerId))
    const requiredAcks = $derived(requiredConnectionAcknowledgements(providerId, strategy))
    const googleAndroid = $derived(isTauriAndroid && providerId === 'google_drive')
    const visibleFields = $derived(definition.fields.filter(field => (
        field.key !== 'oauthRedirectUri' || googleAndroid
    )))
    const authorizationAvailable = $derived(
        providerDescriptors.find(provider => provider.id === providerId)?.authorizationAvailable ?? true,
    )

    $effect(() => onbusychange(busy))

    onMount(async () => {
        try {
            providerDescriptors = await bridge.listProviders()
        } catch {
            error = strings.failed
        }
    })
    onDestroy(() => {
        destroyed = true
        const authorizationId = pendingAuthorizationId
        if (authorizationId && !authorizationCompletionInFlight) {
            void cancelNativeAuthorization(authorizationId)
        }
        onbusychange(false)
    })

    function cancelNativeAuthorization(authorizationId: string): Promise<boolean> {
        if (cancellationId === authorizationId && cancellationPromise) return cancellationPromise
        cancellationId = authorizationId
        cancellationPromise = bridge.cancelAuthorization(authorizationId)
            .then(() => true)
            .catch(() => false)
            .finally(() => {
                if (cancellationId === authorizationId) {
                    cancellationId = null
                    cancellationPromise = null
                }
            })
        return cancellationPromise
    }

    async function cancelPendingAuthorization(): Promise<boolean> {
        if (!pendingAuthorizationId) return true
        const authorizationId = pendingAuthorizationId
        if (await cancelNativeAuthorization(authorizationId)) {
            if (pendingAuthorizationId !== authorizationId) return true
            pendingAuthorizationId = null
            manualOAuthCallback = ''
            authorizationStatus = ''
            return true
        }
        error = strings.failed
        return false
    }

    async function resetPrepared(): Promise<void> {
        if (busy) return
        busy = true
        if (!(await cancelPendingAuthorization())) {
            busy = false
            return
        }
        prepared = null
        endpointConfirmed = false
        currentPlatformClientId = ''
        oauthClientSecret = ''
        manualOAuthCallback = ''
        authorizationStatus = ''
        error = ''
        busy = false
    }

    function selectProvider(value: string): void {
        providerId = value as ExternalProviderId
        const next = getExternalProviderDefinition(providerId)
        strategy = purpose === 'backup'
            ? 'backup-only'
            : next.strategies.includes('sequential') ? 'sequential' : next.strategies[0]
        values = {
            space: 'drive', accountType: 'personal', tenant: 'common',
            ...(isTauriAndroid ? {
                oauthRedirectUri: 'https://update.rsyumi.workers.dev/oauth/google-drive-callback.html',
            } : {}),
            uploadEndpoint: 'https://uploads.github.com', tokenKind: 'personalAccessToken',
            profile: next.profiles[0]?.value ?? '',
        }
        accepted = []
        resetPrepared()
    }

    function updateValue(key: string, value: string): void {
        values[key] = value
        resetPrepared()
    }

    function toggleAcknowledgement(id: string, checked: boolean): void {
        accepted = checked ? [...new Set([...accepted, id])] : accepted.filter(item => item !== id)
        resetPrepared()
    }

    async function prepare(): Promise<void> {
        busy = true
        error = ''
        let request: ReturnType<typeof buildPrepareConnectionRequest>
        try {
            request = buildPrepareConnectionRequest({
                providerId, values, platform, mode, purpose, strategy,
                scope: {
                    ...defaultExternalStorageScope(purpose),
                    deviceSettings: purpose === 'backup' && deviceSettings,
                    devicePlugins: purpose === 'backup' && devicePlugins,
                },
                acknowledgements: accepted,
            })
        } catch {
            error = strings.invalidConfiguration
            busy = false
            return
        }
        try {
            prepared = await bridge.prepareConnection(request)
        } catch {
            error = strings.failed
        } finally {
            busy = false
        }
    }

    async function authenticateRecovery(): Promise<void> {
        busy = true
        error = ''
        try {
            prepared = await bridge.prepareRecoveryImport(recoveryPayload.trim(), recoveryCode.trim())
            providerId = prepared.endpoint.providerId
        } catch {
            error = strings.failed
        } finally {
            busy = false
        }
    }

    async function connect(): Promise<void> {
        if (!prepared || !endpointConfirmed) return
        busy = true
        error = ''
        try {
            if (prepared.requiresOAuth) {
                if (!pendingAuthorizationId) {
                    const pending = await bridge.beginAuthorization(
                        prepared.preparationId,
                        prepared.requiresPlatformOAuthClient
                            ? currentPlatformClientId.trim()
                            : undefined,
                    )
                    if (destroyed) {
                        await cancelNativeAuthorization(pending.authorizationId)
                        return
                    }
                    pendingAuthorizationId = pending.authorizationId
                    authorizationStatus = strings.authorizationWaiting
                    if (pending.authorizationUrl) await openUrl(pending.authorizationUrl)
                    return
                }
                authorizationCompletionInFlight = true
                const result = await bridge.completeAuthorization(
                    pendingAuthorizationId,
                    manualOAuthCallback.trim() || undefined,
                    oauthClientSecret || undefined,
                ).finally(() => authorizationCompletionInFlight = false)
                if (destroyed) {
                    if ('authorizationPending' in result) await cancelPendingAuthorization()
                    return
                }
                if ('authorizationPending' in result) {
                    authorizationStatus = result.callbackRejected
                        ? strings.callbackRejected
                        : strings.authorizationWaiting
                    return
                }
                oauthClientSecret = ''
                manualOAuthCallback = ''
                pendingAuthorizationId = null
                authorizationStatus = ''
                await onconnected(result)
                return
            }
            const secret = buildProviderSecret(providerId, values)
            if (!secret) throw new Error('This provider requires OAuth authorization.')
            await onconnected(await bridge.commitConnection(prepared.preparationId, secret))
            for (const field of definition.secretFields) values[field.key] = ''
        } catch {
            await cancelPendingAuthorization()
            error = strings.failed
        } finally {
            busy = false
        }
    }
</script>

<fieldset disabled={busy} class="space-y-4 p-4" data-external-storage-connection-form>
    <fieldset disabled={prepared !== null} class="contents">
    <div class="grid gap-3 @md:grid-cols-2">
        <label class="space-y-1 text-sm"><span>{strings.provider}</span>
            <SelectInput value={providerId} className="w-full" onchange={event => selectProvider(event.currentTarget.value)}>
                {#each externalProviderDefinitions as provider}<option value={provider.id} disabled={providerDescriptors.find(item => item.id === provider.id)?.authorizationAvailable === false}>{provider.name}</option>{/each}
            </SelectInput>
        </label>
        <label class="space-y-1 text-sm"><span>{strings.create}</span>
            <SelectInput bind:value={mode} className="w-full" onchange={resetPrepared}>
                <option value="create">{strings.create}</option><option value="existing">{strings.existing}</option>
            </SelectInput>
        </label>
        <label class="space-y-1 text-sm"><span>{strings.backup}</span>
            <SelectInput bind:value={purpose} className="w-full" onchange={() => { strategy = purpose === 'backup' ? 'backup-only' : definition.strategies.includes('sequential') ? 'sequential' : definition.strategies[0]; resetPrepared() }}>
                <option value="backup">{strings.backup}</option>
                {#if definition.strategies.some(item => item !== 'backup-only')}<option value="sync">{strings.sync}</option>{/if}
            </SelectInput>
        </label>
        <label class="space-y-1 text-sm"><span>{strings.strategy}</span>
            <SelectInput bind:value={strategy} className="w-full" onchange={() => { purpose = strategy === 'backup-only' ? 'backup' : 'sync'; resetPrepared() }}>
                {#each definition.strategies as item}<option value={item}>{item === 'cas' ? strings.cas : item === 'sequential' ? strings.sequential : strings.backupOnly}</option>{/each}
            </SelectInput>
        </label>
    </div>

    <p class="text-sm text-textcolor2">{definition.description}</p>
    {#if !authorizationAvailable}<p class="rounded-md bg-bgcolor p-2 text-sm text-draculared">{strings.authorizationUnavailable}</p>{/if}
    {#if googleAndroid}<p class="rounded-md bg-bgcolor p-2 text-sm text-textcolor2">{strings.googleAndroidSetup}</p>{/if}
    {#if isTauriAndroid && providerId === 'onedrive'}<p class="rounded-md bg-bgcolor p-2 text-sm text-textcolor2">{strings.oneDriveAndroidSetup}</p>{/if}
    {#if definition.warning}<p class="rounded-md bg-bgcolor p-2 text-sm text-textcolor2">{definition.warning}</p>{/if}

    <div class="grid gap-3 @md:grid-cols-2">
        <label class="space-y-1 text-sm"><span>{strings.endpoint}</span><TextInput fullwidth value={values.endpoint ?? definition.defaultEndpoint} onchange={event => updateValue('endpoint', event.currentTarget.value)} placeholder={definition.defaultEndpoint || 'https://…'} /></label>
        {#if definition.profiles.length}
            <label class="space-y-1 text-sm"><span>{strings.profile}</span><SelectInput value={values.profile ?? definition.profiles[0].value} className="w-full" onchange={event => updateValue('profile', event.currentTarget.value)}>{#each definition.profiles as profile}<option value={profile.value}>{profile.label}</option>{/each}</SelectInput></label>
        {/if}
        {#each visibleFields as field}
            <label class="space-y-1 text-sm"><span>{field.key === 'clientId' && googleAndroid ? strings.webOAuthClientId : field.key === 'oauthRedirectUri' ? strings.oauthCallbackUrl : field.label}{field.required ? ' *' : ''}</span>
                {#if field.type === 'select'}
                    <SelectInput value={values[field.key] ?? field.options?.[0]?.value ?? ''} className="w-full" onchange={event => updateValue(field.key, event.currentTarget.value)}>{#each field.options ?? [] as option}<option value={option.value}>{option.label}</option>{/each}</SelectInput>
                {:else}
                    <TextInput fullwidth value={values[field.key] ?? ''} onchange={event => updateValue(field.key, event.currentTarget.value)} placeholder={field.placeholder ?? ''} />
                {/if}
            </label>
        {/each}
    </div>

    <fieldset class="space-y-2" disabled={purpose === 'sync'}>
        <legend class="mb-1 text-sm font-semibold">{strings.scope}</legend>
        <label class="flex items-center gap-2 text-sm"><input type="checkbox" checked disabled /> {strings.library}</label>
        <label class="flex items-center gap-2 text-sm"><input type="checkbox" bind:checked={deviceSettings} onchange={resetPrepared} /> {strings.deviceSettings}</label>
        <label class="flex items-center gap-2 text-sm"><input type="checkbox" bind:checked={devicePlugins} onchange={resetPrepared} /> {strings.devicePlugins}</label>
    </fieldset>

    {#each requiredAcks as acknowledgement}
        <label class="flex items-start gap-2 rounded-md border border-darkborderc p-3 text-sm">
            <input type="checkbox" checked={accepted.includes(acknowledgement)} onchange={event => toggleAcknowledgement(acknowledgement, event.currentTarget.checked)} />
            <span>{acknowledgement === SEQUENTIAL_ACKNOWLEDGEMENT ? strings.sequentialWarning : acknowledgement === BACKUP_ONLY_ACKNOWLEDGEMENT ? strings.backupWarning : strings.githubWarning}</span>
        </label>
    {/each}
    </fieldset>

    {#if !prepared}
        <p class="text-xs text-textcolor2">{strings.pendingVerification}</p>
        <div class="flex gap-2"><Button disabled={busy || !authorizationAvailable || requiredAcks.some(item => !accepted.includes(item))} onclick={prepare}>{strings.prepare}</Button><Button styled="outlined" onclick={oncancel}>{strings.cancel}</Button></div>
    {:else}
        <div class="rounded-lg border border-selected bg-bgcolor p-3">
            <h4 class="font-semibold">{strings.endpointReview}</h4>
            <dl class="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm"><dt class="text-textcolor2">{strings.authority}</dt><dd class="break-all">{prepared.endpoint.authority}</dd><dt class="text-textcolor2">{strings.repository}</dt><dd class="break-all">{prepared.endpoint.repositoryHint}</dd></dl>
            {#each prepared.endpoint.warnings as warning}<p class="mt-2 text-sm text-textcolor2">{warning}</p>{/each}
            <p class="mt-2 text-xs text-textcolor2">{strings.pendingVerification}</p>
        </div>

        {#if prepared.requiresRecoveryKey}
            <p class="text-sm">{strings.recoveryRequired}</p>
            <label class="block space-y-1 text-sm"><span>{strings.recoveryPayload}</span><textarea class="min-h-24 w-full rounded-md border border-darkborderc bg-transparent p-2" bind:value={recoveryPayload}></textarea></label>
            <label class="block space-y-1 text-sm"><span>{strings.recoveryCode}</span><TextInput fullwidth hideText bind:value={recoveryCode} /></label>
            <Button disabled={busy || !recoveryPayload.trim() || !recoveryCode.trim()} onclick={authenticateRecovery}>{strings.unlock}</Button>
        {:else}
            <label class="flex items-center gap-2 text-sm"><input type="checkbox" bind:checked={endpointConfirmed} /> {strings.confirmEndpoint}</label>
            {#if prepared.requiresPlatformOAuthClient}
                <label class="block space-y-1 text-sm"><span>{googleAndroid ? strings.webOAuthClientId : strings.platformClientId}</span><TextInput fullwidth bind:value={currentPlatformClientId} /></label>
                {#if prepared.oauthProjectHint}<p class="text-xs text-textcolor2">{strings.oauthProjectHint}: {prepared.oauthProjectHint}</p>{/if}
            {/if}
            {#if endpointConfirmed && prepared.requiresOAuth && googleAndroid}
                <div class="grid gap-3 @md:grid-cols-2">
                    <label class="space-y-1 text-sm"><span>{strings.oauthClientSecret}</span><TextInput fullwidth hideText bind:value={oauthClientSecret} /></label>
                    {#if pendingAuthorizationId}<label class="space-y-1 text-sm"><span>{strings.manualOAuthCallback}</span><TextInput fullwidth hideText bind:value={manualOAuthCallback} /></label>{/if}
                </div>
                {#if pendingAuthorizationId}<p class="text-xs text-textcolor2">{strings.manualOAuthHelp}</p>{/if}
            {/if}
            {#if endpointConfirmed && !prepared.requiresOAuth}
                <div class="grid gap-3 @md:grid-cols-2">{#each definition.secretFields as field}<label class="space-y-1 text-sm"><span>{field.label}</span>{#if field.type === 'select'}<SelectInput value={values[field.key] ?? field.options?.[0]?.value ?? ''} className="w-full" onchange={event => values[field.key] = event.currentTarget.value}>{#each field.options ?? [] as option}<option value={option.value}>{option.label}</option>{/each}</SelectInput>{:else if field.type === 'datetime-local'}<input type="datetime-local" class="w-full rounded-md border border-darkborderc bg-transparent p-2" bind:value={values[field.key]} />{:else}<TextInput fullwidth hideText={field.secret} bind:value={values[field.key]} />{/if}</label>{/each}</div>
            {/if}
            {#if authorizationStatus}<p class="text-sm text-textcolor2" role="status">{authorizationStatus}</p>{/if}
            <div class="flex flex-wrap gap-2"><Button disabled={busy || !endpointConfirmed || !authorizationAvailable || (prepared.requiresPlatformOAuthClient && !currentPlatformClientId.trim())} onclick={connect}>{prepared.requiresOAuth ? (pendingAuthorizationId ? strings.finishSignIn : strings.signIn) : strings.connect}</Button><Button styled="outlined" onclick={resetPrepared}>{strings.cancel}</Button></div>
        {/if}
    {/if}
    {#if error}<p class="text-sm text-draculared" role="alert">{error}</p>{/if}
</fieldset>
