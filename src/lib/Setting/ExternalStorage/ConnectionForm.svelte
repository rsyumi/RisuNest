<script lang="ts">
    import { onDestroy, onMount } from 'svelte'
    import TextInput from 'src/lib/UI/GUI/TextInput.svelte'
    import SelectInput from 'src/lib/UI/GUI/SelectInput.svelte'
    import OptionInput from 'src/lib/UI/GUI/OptionInput.svelte'
    import SegmentedButtons from '../RisuNest/SegmentedButtons.svelte'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import { openUrl } from '@tauri-apps/plugin-opener'
    import { type as osType } from '@tauri-apps/plugin-os'
    import { isTauriAndroid, isTauriIOS } from 'src/ts/platform'
    import { getExternalStorageBridge } from 'src/ts/storage/sync/external/bridge'
    import {
        buildPrepareConnectionRequest,
        defaultExternalCapturePolicy,
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
        PreparedExternalConnection,
    } from 'src/ts/storage/sync/external/types'
    import {
        externalEndpointWarning,
        externalErrorMessage,
        externalFieldHelp,
        externalFieldLabel,
        externalOptionLabel,
        externalProfileLabel,
        externalProviderName,
        type ExternalStorageStrings,
    } from './strings'

    interface Props {
        strings: ExternalStorageStrings
        onconnected: (result: ExternalConnectionResult) => void | Promise<void>
        oncancel: () => void
        onbusychange?: (busy: boolean) => void
        /** Opens an already existing repository only, without the create form. */
        restoreOnly?: boolean
        tone?: 'settings' | 'onboarding'
    }

    let {
        strings,
        onconnected,
        oncancel,
        onbusychange = () => {},
        restoreOnly = false,
        tone = 'settings',
    }: Props = $props()
    const bridge = getExternalStorageBridge()
    const platform = isTauriAndroid
        ? 'android'
        : isTauriIOS
          ? 'ios'
          : ({ windows: 'windows', macos: 'macos', linux: 'linux' } as Record<string, string>)[osType()] ?? 'windows'
    let providerId = $state<ExternalProviderId>('google_drive')
    let chosenMode = $state<ExternalOpenMode>('create')
    const mode = $derived<ExternalOpenMode>(restoreOnly ? 'existing' : chosenMode)
    // The recovery file carries the repository settings, so the review must not
    // show the purpose this form never asked for.
    let fromRecovery = $state(false)
    let purpose = $state<ExternalConnectionPurpose>('backup')
    let values = $state<Record<string, string>>({
        space: 'drive', accountType: 'personal', tenant: 'common',
        ...(isTauriAndroid ? {
            oauthRedirectUri: 'https://update.rsyumi.workers.dev/oauth/google-drive-callback.html',
        } : {}),
    })
    let hypa = $state(true)
    let localPlugins = $state(true)
    let localSettings = $state(true)
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
    const requiredAcks = $derived(requiredConnectionAcknowledgements(providerId))
    const googleAndroid = $derived(isTauriAndroid && providerId === 'google_drive')
    const visibleFields = $derived(definition.fields.filter(field => (
        field.key !== 'oauthRedirectUri' || googleAndroid
    )))
    const authorizationAvailable = $derived(
        providerDescriptors.find(provider => provider.id === providerId)?.authorizationAvailable ?? true,
    )
    const providerStrings = $derived(strings.providers[providerId])
    const supportsSync = $derived(definition.supportsSync)
    const providerOptions = $derived(externalProviderDefinitions.map(provider => ({
        value: provider.id,
        label: externalProviderName(strings, provider.id),
        disabled: providerDescriptors.find(item => item.id === provider.id)?.authorizationAvailable === false,
    })))
    const purposeOptions = $derived([
        { value: 'backup' as const, label: strings.backup },
        ...(supportsSync ? [{ value: 'sync' as const, label: strings.sync }] : []),
    ])
    const scopeSummary = $derived([
        strings.library,
        ...(hypa ? [strings.hypa] : []),
        ...(localPlugins ? [strings.devicePlugins] : []),
        ...(localSettings ? [strings.deviceSettings] : []),
    ].join(', '))
    const providerWarning = $derived('warningTitle' in providerStrings
        ? { title: providerStrings.warningTitle, body: providerStrings.warning }
        : null)
    const connectLabel = $derived(prepared?.requiresOAuth
        ? (pendingAuthorizationId ? strings.finishSignIn : strings.signIn)
        : strings.connect)

    $effect(() => onbusychange(busy))

    onMount(async () => {
        try {
            providerDescriptors = await bridge.listProviders()
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
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
        error = strings.errorGeneric
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
        fromRecovery = false
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
        if (!next.supportsSync) purpose = 'backup'
        values = {
            space: 'drive', accountType: 'personal', tenant: 'common',
            ...(isTauriAndroid ? {
                oauthRedirectUri: 'https://update.rsyumi.workers.dev/oauth/google-drive-callback.html',
            } : {}),
            uploadEndpoint: 'https://uploads.github.com',
            profile: next.profiles[0]?.value ?? '',
        }
        accepted = []
        resetPrepared()
    }

    function selectMode(value: ExternalOpenMode): void {
        chosenMode = value
        recoveryPayload = ''
        recoveryCode = ''
        resetPrepared()
    }

    function readRecoveryFile(): Promise<string | null> {
        return new Promise(resolve => {
            const input = document.createElement('input')
            input.type = 'file'
            // The Android picker hides extensions it does not know, so it stays
            // open there and the contents are checked when the key is read.
            if (!isTauriAndroid) input.accept = '.rnrecovery,text/plain'
            input.style.display = 'none'
            const finish = async (file?: File) => {
                input.remove()
                resolve(file ? await file.text() : null)
            }
            input.addEventListener('cancel', () => void finish())
            input.addEventListener('change', () => void finish(input.files?.[0] ?? undefined))
            document.body.appendChild(input)
            input.click()
        })
    }

    async function loadRecoveryFile(): Promise<void> {
        const contents = await readRecoveryFile()
        if (contents === null) return
        recoveryPayload = contents.replace(/^﻿/, '').trim()
        error = ''
    }

    function selectPurpose(value: ExternalConnectionPurpose): void {
        purpose = value
        resetPrepared()
    }

    function fieldLabel(key: string): string {
        if (key === 'clientId' && googleAndroid) return strings.webOAuthClientId
        if (key === 'clientId' && isTauriIOS) return strings.iosOAuthClientId
        if (key === 'oauthRedirectUri') return strings.oauthCallbackUrl
        return externalFieldLabel(strings, providerId, key)
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
                providerId, values, platform, mode, purpose,
                capturePolicy: purpose === 'backup'
                    ? { hypa, localPlugins, localSettings }
                    : defaultExternalCapturePolicy(purpose),
                acknowledgements: accepted,
            })
        } catch {
            error = strings.invalidConfiguration
            busy = false
            return
        }
        try {
            prepared = await bridge.prepareConnection(request)
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }

    async function authenticateRecovery(): Promise<void> {
        busy = true
        error = ''
        try {
            prepared = await bridge.prepareRecoveryImport(recoveryPayload.trim(), recoveryCode.trim())
            fromRecovery = true
            providerId = prepared.endpoint.providerId
        } catch (reason) {
            error = externalErrorMessage(strings, reason)
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
                    authorizationStatus = pending.state === 'complete'
                        ? ''
                        : strings.authorizationWaiting
                    if (pending.authorizationUrl) {
                        await openUrl(pending.authorizationUrl)
                        return
                    }
                    if (pending.state !== 'complete') return
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
        } catch (reason) {
            await cancelPendingAuthorization()
            error = externalErrorMessage(strings, reason)
        } finally {
            busy = false
        }
    }
</script>

<fieldset disabled={busy} class="form" data-external-storage-connection-form data-tone={tone}>
    <fieldset disabled={prepared !== null} class="contents">
    {#if !restoreOnly}
    <section class="sub">
        <h4 class="sub-title">{mode === 'create' ? strings.provider : strings.mode}</h4>
        <div class="fields two">
            {#if mode === 'create'}
                <label class="field">
                    <span>{strings.provider}</span>
                    <SelectInput value={providerId} className="w-full" onchange={event => selectProvider(event.currentTarget.value)}>
                        {#each providerOptions as option (option.value)}<OptionInput value={option.value} disabled={option.disabled}>{option.label}</OptionInput>{/each}
                    </SelectInput>
                    <small>{providerStrings.description}</small>
                </label>
            {/if}
            <div class="field">
                <span>{strings.mode}</span>
                <SegmentedButtons value={mode} label={strings.mode} role="radiogroup" onchange={selectMode} options={[{ value: 'create', label: strings.create }, { value: 'existing', label: strings.existing }]} />
                {#if mode === 'existing'}<small>{strings.existingHelp}</small>{/if}
            </div>
        </div>
        {#if mode === 'create'}
            {#if !authorizationAvailable}<p class="note danger"><span>{strings.authorizationUnavailable}</span></p>{/if}
            {#if googleAndroid}<p class="note"><span>{strings.googleAndroidSetup}</span></p>{/if}
            {#if isTauriAndroid && providerId === 'onedrive'}<p class="note"><span>{strings.oneDriveAndroidSetup}</span></p>{/if}
            {#if isTauriIOS && providerId === 'google_drive'}<p class="note"><span>{strings.googleIOSSetup}</span></p>{/if}
            {#if isTauriIOS && providerId === 'onedrive'}<p class="note"><span>{strings.oneDriveIOSSetup}</span></p>{/if}
        {/if}
    </section>
    {/if}

    {#if mode === 'create'}
    <section class="sub">
        <h4 class="sub-title">{strings.purpose}</h4>
        <div class="field">
            <SegmentedButtons value={purpose} label={strings.purpose} role="radiogroup" onchange={selectPurpose} options={purposeOptions} />
            {#if !supportsSync}<small>{strings.backupOnlyProvider}</small>{/if}
            <small>{strings.purposeHelp}</small>
        </div>
        {#if purpose === 'sync'}
            <div class="field">
                <small>{strings.sequentialWarning}</small>
            </div>
        {/if}
        {#if providerWarning}
            <div class="warning">
                <strong>{providerWarning.title}</strong>
                <p>{providerWarning.body}</p>
            </div>
        {/if}
        {#each requiredAcks as acknowledgement (acknowledgement)}
            <div class="warning">
                <strong>{strings.githubWarningTitle}</strong>
                <p>{strings.githubWarning}</p>
                <label class="check">
                    <input type="checkbox" checked={accepted.includes(acknowledgement)} onchange={event => toggleAcknowledgement(acknowledgement, event.currentTarget.checked)} />
                    <span>{strings.acknowledge}</span>
                    <span class="sr-only">{strings.githubWarning}</span>
                </label>
            </div>
        {/each}
    </section>

    <section class="sub">
        <h4 class="sub-title">{strings.connectionInfo}</h4>
        <div class="fields two">
            {#if definition.customEndpoint}
                <label class="field span2"><span>{strings.endpoint}</span><TextInput fullwidth value={values.endpoint ?? definition.defaultEndpoint} onchange={event => updateValue('endpoint', event.currentTarget.value)} placeholder={definition.defaultEndpoint || 'https://…'} /></label>
            {/if}
            {#if definition.profiles.length > 1}
                <label class="field"><span>{strings.profile}</span>
                    <SelectInput value={values.profile ?? definition.profiles[0].value} className="w-full" onchange={event => updateValue('profile', event.currentTarget.value)}>
                        {#each definition.profiles as profile (profile.value)}<OptionInput value={profile.value}>{externalProfileLabel(strings, providerId, profile.value, profile.label)}</OptionInput>{/each}
                    </SelectInput>
                </label>
            {/if}
            {#each visibleFields as field (field.key)}
                {@const help = externalFieldHelp(strings, providerId, field.key)}
                <label class="field">
                    <span>{fieldLabel(field.key)}{field.required ? '' : strings.optional}</span>
                    {#if field.type === 'select'}
                        <SelectInput value={values[field.key] ?? field.options?.[0] ?? ''} className="w-full" onchange={event => updateValue(field.key, event.currentTarget.value)}>
                            {#each field.options ?? [] as option (option)}<OptionInput value={option}>{externalOptionLabel(strings, providerId, field.key, option)}</OptionInput>{/each}
                        </SelectInput>
                    {:else}
                        <TextInput fullwidth value={values[field.key] ?? ''} onchange={event => updateValue(field.key, event.currentTarget.value)} placeholder={field.placeholder ?? ''} />
                    {/if}
                    {#if help}<small>{help}</small>{/if}
                </label>
            {/each}
        </div>
    </section>

    {#if purpose === 'backup'}
    <fieldset class="sub">
        <legend class="sub-title">{strings.scope}</legend>
        <p class="note"><span>{strings.scopeHelp}</span></p>
        <p class="check fixed"><span>{strings.library}</span><span class="value">{strings.included}</span></p>
        <p class="note"><span>{strings.libraryHelp}</span></p>
        <label class="check"><input type="checkbox" bind:checked={hypa} onchange={resetPrepared} /><span>{strings.hypa}</span></label>
        <p class="note"><span>{strings.hypaHelp}</span></p>
        <label class="check"><input type="checkbox" bind:checked={localPlugins} onchange={resetPrepared} /><span>{strings.devicePlugins}</span></label>
        <p class="note"><span>{strings.devicePluginsHelp}</span></p>
        <label class="check"><input type="checkbox" bind:checked={localSettings} onchange={resetPrepared} /><span>{strings.deviceSettings}</span></label>
        <p class="note"><span>{strings.deviceSettingsHelp}</span></p>
    </fieldset>
    {/if}
    {/if}
    </fieldset>

    {#if prepared}
        <section class="sub">
            <h4 class="sub-title">{strings.endpointReview}</h4>
            <dl class="review">
                <dt>{strings.authority}</dt><dd>{prepared.endpoint.authority}</dd>
                {#if prepared.endpoint.accountHint}<dt>{strings.account}</dt><dd>{prepared.endpoint.accountHint}</dd>{/if}
                <dt>{strings.repository}</dt><dd>{prepared.endpoint.repositoryHint}</dd>
                {#if !fromRecovery}
                    <dt>{strings.purposeReview}</dt>
                    <dd>{purpose === 'backup' ? `${strings.backup} · ${strings.includes.replace('{0}', scopeSummary)}` : strings.sync}</dd>
                {/if}
                {#if prepared.requiresPlatformOAuthClient && prepared.oauthProjectHint}<dt>{strings.oauthProjectHint}</dt><dd>{prepared.oauthProjectHint}</dd>{/if}
            </dl>
            {#if purpose === 'sync' && !fromRecovery}
                <p class="note"><span>{strings.syncPurposeLocalDataNotice}</span></p>
            {/if}
            {#each prepared.endpoint.warnings as warning (warning)}<p class="note"><span>{externalEndpointWarning(strings, warning)}</span></p>{/each}

            <label class="check"><input type="checkbox" bind:checked={endpointConfirmed} /><span>{strings.confirmEndpoint}</span></label>
            {#if prepared.requiresPlatformOAuthClient}
                <label class="field"><span>{googleAndroid ? strings.webOAuthClientId : strings.platformClientId}</span><TextInput fullwidth bind:value={currentPlatformClientId} /></label>
            {/if}
            {#if endpointConfirmed && prepared.requiresOAuth && googleAndroid}
                <div class="fields two">
                    <label class="field"><span>{strings.oauthClientSecret}</span><TextInput fullwidth hideText bind:value={oauthClientSecret} /></label>
                    {#if pendingAuthorizationId}<label class="field"><span>{strings.manualOAuthCallback}</span><TextInput fullwidth hideText bind:value={manualOAuthCallback} /><small>{strings.manualOAuthHelp}</small></label>{/if}
                </div>
            {/if}
            {#if endpointConfirmed && !prepared.requiresOAuth}
                <div class="fields two">
                    {#each definition.secretFields as field (field.key)}
                        {@const help = externalFieldHelp(strings, providerId, field.key)}
                        <label class="field">
                            <span>{fieldLabel(field.key)}</span>
                            {#if field.type === 'select'}
                                <SelectInput value={values[field.key] ?? field.options?.[0] ?? ''} className="w-full" onchange={event => values[field.key] = event.currentTarget.value}>
                                    {#each field.options ?? [] as option (option)}<OptionInput value={option}>{externalOptionLabel(strings, providerId, field.key, option)}</OptionInput>{/each}
                                </SelectInput>
                            {:else if field.type === 'datetime-local'}
                                <input type="datetime-local" class="datetime" bind:value={values[field.key]} />
                            {:else}
                                <TextInput fullwidth hideText={field.secret} bind:value={values[field.key]} />
                            {/if}
                            {#if help}<small>{help}</small>{/if}
                        </label>
                    {/each}
                </div>
            {/if}
            {#if authorizationStatus}<p class="sub-help" role="status">{authorizationStatus}</p>{/if}
            <div class="actions">
                <SettingButton disabled={busy || !endpointConfirmed || !authorizationAvailable || (prepared.requiresPlatformOAuthClient && !currentPlatformClientId.trim())} onclick={connect}>{connectLabel}</SettingButton>
                <SettingButton variant="secondary" onclick={resetPrepared}>{strings.back}</SettingButton>
            </div>
            {#if !prepared.requiresOAuth && mode === 'create'}<p class="sub-help">{strings.connectHint}</p>{/if}
        </section>
    {:else if mode === 'existing'}
        <section class="sub">
            <h4 class="sub-title">{strings.recoveryRequiredTitle}</h4>
            <p class="sub-help">{strings.recoveryRequired}</p>
            <div class="actions">
                <SettingButton variant="secondary" disabled={busy} onclick={loadRecoveryFile}>{strings.openRecoveryFile}</SettingButton>
            </div>
            <label class="field"><span>{strings.recoveryPayload}</span><textarea class="textarea" placeholder={strings.recoveryPayloadPlaceholder} bind:value={recoveryPayload}></textarea></label>
            <label class="field"><span>{strings.recoveryCode}</span><TextInput fullwidth hideText bind:value={recoveryCode} /></label>
            <div class="actions">
                <SettingButton disabled={busy || !recoveryPayload.trim() || !recoveryCode.trim()} onclick={authenticateRecovery}>{strings.unlock}</SettingButton>
                <SettingButton variant="secondary" onclick={oncancel}>{strings.cancel}</SettingButton>
            </div>
        </section>
    {:else}
        <section class="sub">
            <div class="actions">
                <SettingButton disabled={busy || !authorizationAvailable || requiredAcks.some(item => !accepted.includes(item))} onclick={prepare}>{strings.prepare}</SettingButton>
                <SettingButton variant="secondary" onclick={oncancel}>{strings.cancel}</SettingButton>
            </div>
            <p class="sub-help">{strings.pendingVerification}</p>
        </section>
    {/if}
    {#if error}<p class="px-4 pb-4 text-sm text-danger-400" role="alert">{error}</p>{/if}
</fieldset>

<style>
    .form {
        display: grid;
        min-width: 0;
    }
    .form > .sub,
    .form > .contents > .sub + .sub {
        border-top: 1px solid color-mix(in srgb, var(--risu-theme-darkborderc) 55%, transparent);
    }
    /* The onboarding panel draws its own frame and gutter. */
    .form[data-tone='onboarding'] > .sub,
    .form[data-tone='onboarding'] > .contents > .sub + .sub {
        border-top: 0;
    }
    .form[data-tone='onboarding'] .sub {
        padding: 0 0 0.75rem;
    }
    .sub {
        display: grid;
        gap: 0.75rem;
        padding: 0.9rem 1rem 1rem;
        min-width: 0;
    }
    .sub-title {
        margin: 0;
        font-size: 0.75rem;
        font-weight: 600;
        letter-spacing: 0.04em;
        text-transform: uppercase;
        color: color-mix(in srgb, var(--risu-theme-textcolor) 60%, transparent);
    }
    .fields {
        display: grid;
        gap: 0.75rem;
        grid-template-columns: minmax(0, 1fr);
    }
    @container (min-width: 40rem) {
        .fields.two {
            grid-template-columns: repeat(2, minmax(0, 1fr));
        }
        .fields.two .span2 {
            grid-column: 1 / -1;
        }
    }
    .field {
        display: grid;
        align-content: start;
        gap: 0.35rem;
        min-width: 0;
        font-size: 0.875rem;
    }
    .field > span {
        font-weight: 500;
    }
    .field > small,
    .sub-help {
        margin: 0;
        font-size: 0.8125rem;
        line-height: 1.45;
        color: var(--risu-theme-textcolor2);
    }
    .check {
        display: flex;
        align-items: flex-start;
        gap: 0.5rem;
        font-size: 0.875rem;
    }
    .check input {
        margin-top: 0.2rem;
    }
    .check.fixed {
        margin: 0;
        justify-content: space-between;
    }
    .check.fixed .value {
        color: var(--risu-theme-textcolor2);
    }
    .actions {
        display: flex;
        flex-wrap: wrap;
        gap: 0.5rem;
    }
    .note {
        display: grid;
        margin: 0;
        padding: 0.65rem 0.85rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
        font-size: 0.8125rem;
        line-height: 1.45;
        color: var(--risu-theme-textcolor2);
    }
    .note.danger {
        color: var(--risu-theme-danger-400);
        border-color: color-mix(in srgb, var(--risu-theme-danger-400) 45%, transparent);
    }
    .warning {
        display: grid;
        gap: 0.4rem;
        padding: 0.85rem 1rem;
        border: 1px solid color-mix(in srgb, var(--risu-theme-danger-400) 45%, transparent);
        border-left-width: 3px;
        border-radius: 0.5rem;
        background: color-mix(in srgb, var(--risu-theme-danger-400) 6%, transparent);
        font-size: 0.875rem;
    }
    .warning strong {
        font-weight: 600;
    }
    .warning p {
        margin: 0;
        font-size: 0.8125rem;
        line-height: 1.45;
        opacity: 0.85;
    }
    .warning .check {
        margin-top: 0.15rem;
    }
    .review {
        display: grid;
        grid-template-columns: minmax(0, 1fr);
        gap: 0.15rem 1.1rem;
        margin: 0;
        padding: 0.85rem 1rem;
        border: 1px solid rgba(34, 200, 198, 0.35);
        border-radius: 0.75rem;
        background: rgba(34, 200, 198, 0.08);
        font-size: 0.85rem;
    }
    .review dt {
        font-size: 0.78rem;
        color: var(--risu-theme-textcolor2);
    }
    .review dd {
        margin: 0 0 0.5rem;
        min-width: 0;
        overflow-wrap: anywhere;
        font-weight: 600;
    }
    .review dd:last-child {
        margin-bottom: 0;
    }
    .textarea,
    .datetime {
        width: 100%;
        min-width: 0;
        padding: 0.5rem 0.75rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.375rem;
        background: transparent;
        color: inherit;
        font: inherit;
    }
    .textarea {
        min-height: 6rem;
        resize: vertical;
    }
</style>
