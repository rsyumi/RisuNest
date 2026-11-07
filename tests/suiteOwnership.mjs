import { readdir } from 'node:fs/promises'
import { join } from 'node:path'

export const testSuffix = '**/*.{test,spec}.?(c|m)[jt]s?(x)'
export const appRoots = ['src', 'tests', 'scripts', 'src-tauri/src']
export const appTestIncludes = appRoots.map(root => `${root}/${testSuffix}`)
export const luaWorkerTests = ['src/ts/process/scriptings.test.ts', 'src/ts/process/scriptings.historyWindow.test.ts']
export const extendedAppTests = ['src/ts/storage/selectedConversationEvictionCorpus.test.ts']
// Tests that need no DOM run without happy-dom, which skips its per-file setup.
export const nodeAppTests = [
  'scripts/phase3AndroidSmoke.test.ts',
  'src-tauri/src/ios_ipc.test.ts',
  'src-tauri/src/ipc_read_guard.test.ts',
  'src/appUpdateCheck.test.ts',
  'src/lang/checkboxConfirmCopy.test.ts',
  'src/lib/ChatScreens/conversationStartMutations.test.ts',
  'src/lib/ChatScreens/defaultChatInput.test.ts',
  'src/lib/Others/HypaV3Modal/hypaOrphanState.test.ts',
  'src/lib/Others/Onboarding/externalStorageOnboardingFlow.test.ts',
  'src/lib/Others/Onboarding/onboardingFlow.test.ts',
  'src/lib/Others/bookmarkOperation.test.ts',
  'src/lib/Others/modalLayering.test.ts',
  'src/lib/Setting/ExternalStorage/strings.test.ts',
  'src/lib/Setting/Pages/Module/ModuleSettings.persistence.test.ts',
  'src/lib/Setting/Pages/UserSettings.account.test.ts',
  'src/lib/Setting/Pages/UserSettings.localBackupRoute.test.ts',
  'src/lib/Setting/presetOperation.test.ts',
  'src/lib/SideBars/devToolLease.test.ts',
  'src/lib/SideBars/sideChatListRows.test.ts',
  'src/lib/scopedClassNames.test.ts',
  'src/lib/workingSetNavigation.test.ts',
  'src/ts/abortSignals.test.ts',
  'src/ts/alertQueue.test.ts',
  'src/ts/arrayRange.test.ts',
  'src/ts/autoSuggestionMetadata.test.ts',
  'src/ts/bootFailureClassification.test.ts',
  'src/ts/cbs.historyWindow.test.ts',
  'src/ts/cbs.operationContext.test.ts',
  'src/ts/chatBindings.test.ts',
  'src/ts/chatBranchUi.test.ts',
  'src/ts/chatEditorDrafts.test.ts',
  'src/ts/chatLoadPages.test.ts',
  'src/ts/chatMessageUi.test.ts',
  'src/ts/chatParserHistory.test.ts',
  'src/ts/chatParserHistoryProjection.test.ts',
  'src/ts/chatRemoval.test.ts',
  'src/ts/chatRenderIdentity.test.ts',
  'src/ts/chatScreenshotArchive.test.ts',
  'src/ts/chatScreenshotRange.test.ts',
  'src/ts/chatScreenshotSourceLease.test.ts',
  'src/ts/chatViewport.test.ts',
  'src/ts/consoleLog.test.ts',
  'src/ts/conversationMutations.test.ts',
  'src/ts/conversationViewportSource.test.ts',
  'src/ts/deepLinkDispatcher.test.ts',
  'src/ts/desktopNotifications.test.ts',
  'src/ts/desktopRelaunch.test.ts',
  'src/ts/downloadFileName.test.ts',
  'src/ts/drive/accounter.test.ts',
  'src/ts/drive/backupAssets.test.ts',
  'src/ts/drive/legacyBackupAttachments.test.ts',
  'src/ts/drive/pocketRisuBackup.test.ts',
  'src/ts/drive/pocketRisuFeatures.test.ts',
  'src/ts/durableReroll.test.ts',
  'src/ts/gui/branches.test.ts',
  'src/ts/gui/loreListWindow.test.ts',
  'src/ts/gui/nativeFileJobDialogModel.test.ts',
  'src/ts/gui/nativeFileJobProgress.test.ts',
  'src/ts/gui/updateDialogModel.test.ts',
  'src/ts/hotkeyModifier.test.ts',
  'src/ts/htmlClipboardExport.test.ts',
  'src/ts/importUtf8.test.ts',
  'src/ts/iosScreenshotArchiveWriter.test.ts',
  'src/ts/kei/backup.test.ts',
  'src/ts/liveDisplayParserLease.test.ts',
  'src/ts/loadout.test.ts',
  'src/ts/media/tests/imageType.test.ts',
  'src/ts/nativeLocalUrls.test.ts',
  'src/ts/nativeLog.test.ts',
  'src/ts/nativeScreenshotArchiveWriter.test.ts',
  'src/ts/nativeStartup.test.ts',
  'src/ts/network/boundedFetchPreview.test.ts',
  'src/ts/network/localNetwork.test.ts',
  'src/ts/network/networkLog.test.ts',
  'src/ts/network/requestAbortScope.test.ts',
  'src/ts/network/tauriHttpStream.test.ts',
  'src/ts/parser/assetLookup.test.ts',
  'src/ts/parser/boundedTextUnits.test.ts',
  'src/ts/parser/streamingThoughtPreview.test.ts',
  'src/ts/parser/tests/chatML.test.ts',
  'src/ts/persona.test.ts',
  'src/ts/plugins/apiV3/risunestPrivateApi.test.ts',
  'src/ts/plugins/chatViewEvents.test.ts',
  'src/ts/plugins/conversationContext.test.ts',
  'src/ts/plugins/conversationPatch.test.ts',
  'src/ts/plugins/generationEndEvents.test.ts',
  'src/ts/plugins/hostToolBridge.test.ts',
  'src/ts/plugins/pluginAndroidInitBoundary.test.ts',
  'src/ts/plugins/pluginChatOutputListeners.test.ts',
  'src/ts/plugins/pluginClaimSession.test.ts',
  'src/ts/plugins/pluginCompatibility.test.ts',
  'src/ts/plugins/pluginDataInventory.removal.test.ts',
  'src/ts/plugins/pluginDataInventory.search.test.ts',
  'src/ts/plugins/pluginDataInventory.test.ts',
  'src/ts/plugins/pluginDatabaseAccess.test.ts',
  'src/ts/plugins/pluginReadBaselines.test.ts',
  'src/ts/plugins/pluginSafeClass.nativeStorage.test.ts',
  'src/ts/plugins/pluginStorageIsolation.test.ts',
  'src/ts/plugins/pluginStorageStore.test.ts',
  'src/ts/pngChunk.test.ts',
  'src/ts/pocketChatControls.integration.test.ts',
  'src/ts/polyfill.test.ts',
  'src/ts/process/coldPayloadExpansion.test.ts',
  'src/ts/process/coldstorageAccount.test.ts',
  'src/ts/process/coldstorageData.test.ts',
  'src/ts/process/command.test.ts',
  'src/ts/process/conversationOperationContext.test.ts',
  'src/ts/process/currentChatParserPass.test.ts',
  'src/ts/process/files/multisend.test.ts',
  'src/ts/process/files/tests/imageGeometryController.test.ts',
  'src/ts/process/files/tests/imageGeometryJob.test.ts',
  'src/ts/process/files/tests/inlayInventory.test.ts',
  'src/ts/process/files/tests/inlayOptimizationController.svelte.test.ts',
  'src/ts/process/files/tests/inlayOptimizationJob.test.ts',
  'src/ts/process/generationConversationOperation.test.ts',
  'src/ts/process/generationEnd.test.ts',
  'src/ts/process/generationErrorResponse.test.ts',
  'src/ts/process/generationPersistenceBoundary.test.ts',
  'src/ts/process/generationState.test.ts',
  'src/ts/process/historyWindowController.test.ts',
  'src/ts/process/historyWindowGeneration.test.ts',
  'src/ts/process/historyWindowHistory.test.ts',
  'src/ts/process/historyWindowIndex.test.ts',
  'src/ts/process/historyWindowSelection.test.ts',
  'src/ts/process/historyWindowWrite.test.ts',
  'src/ts/process/mcp/stdioApproval.test.ts',
  'src/ts/process/memory/hypaCacheKey.test.ts',
  'src/ts/process/memory/hypaEmbeddingCacheUse.test.ts',
  'src/ts/process/memory/taskRateLimiter.test.ts',
  'src/ts/process/nativeRegexBatch.test.ts',
  'src/ts/process/presetChain.test.ts',
  'src/ts/process/promptHistory.test.ts',
  'src/ts/process/regexExecutionPlan.test.ts',
  'src/ts/process/regexSafePlan.test.ts',
  'src/ts/process/regexWorkerClient.test.ts',
  'src/ts/process/request/anthropic.bedrock.test.ts',
  'src/ts/process/request/openAI/requests.responses.test.ts',
  'src/ts/process/request/request.ollama.test.ts',
  'src/ts/process/request/tests/additionalParams.test.ts',
  'src/ts/process/streamingDisplayScheduler.test.ts',
  'src/ts/process/streamingDisplayStream.test.ts',
  'src/ts/process/summaryAwareGenerationPreparation.test.ts',
  'src/ts/process/summaryAwarePromptHistory.test.ts',
  'src/ts/process/templates/chatTemplate.test.ts',
  'src/ts/process/transformers.resources.test.ts',
  'src/ts/process/transformers.test.ts',
  'src/ts/process/ttsHooks.test.ts',
  'src/ts/realmEndpoints.test.ts',
  'src/ts/responseVariants.test.ts',
  'src/ts/risuLocalUrl.test.ts',
  'src/ts/runtimePerformanceProfile.env.test.ts',
  'src/ts/runtimePerformanceProfile.test.ts',
  'src/ts/selectedConversationLiveParserProjection.test.ts',
  'src/ts/selectedConversationOperations.test.ts',
  'src/ts/selectedConversationTail.test.ts',
  'src/ts/selectedConversationViewportBinding.test.ts',
  'src/ts/setting/fullscreenSettings.test.ts',
  'src/ts/setting/importPresetRegex.test.ts',
  'src/ts/setting/risuNestSettingsData.test.ts',
  'src/ts/setting/streamingDisplaySettings.test.ts',
  'src/ts/sourcemap.test.ts',
  'src/ts/storage/accountAssetAccess.test.ts',
  'src/ts/storage/activeConversationSession.test.ts',
  'src/ts/storage/androidRisuSaveRoute.test.ts',
  'src/ts/storage/androidRisuSaveRouteProduction.test.ts',
  'src/ts/storage/androidSafDownload.test.ts',
  'src/ts/storage/androidSafOwnership.test.ts',
  'src/ts/storage/appCleanup.test.ts',
  'src/ts/storage/assetOwnerCompatibilityProjector.test.ts',
  'src/ts/storage/assetRepositoryRuntime.test.ts',
  'src/ts/storage/assetSourceRoute.test.ts',
  'src/ts/storage/backgroundErrorMessage.test.ts',
  'src/ts/storage/blobStore.test.ts',
  'src/ts/storage/bootstrapJsonSnapshot.test.ts',
  'src/ts/storage/characterArchive.test.ts',
  'src/ts/storage/characterArchiveView.test.ts',
  'src/ts/storage/characterImageFileRoute.test.ts',
  'src/ts/storage/characterOrderMutation.test.ts',
  'src/ts/storage/committedWorkingSetContinuation.test.ts',
  'src/ts/storage/committedWorkingSetRefresh.test.ts',
  'src/ts/storage/compatibleBackupFileRouteProduction.test.ts',
  'src/ts/storage/compatibleBackupReport.test.ts',
  'src/ts/storage/completeAssetRepositoryBlobStore.test.ts',
  'src/ts/storage/conversationBranchJobs.test.ts',
  'src/ts/storage/conversationHistoryOperation.test.ts',
  'src/ts/storage/conversationInsertPages.test.ts',
  'src/ts/storage/dataHealth.test.ts',
  'src/ts/storage/dataHealthModel.test.ts',
  'src/ts/storage/dataHealthNames.test.ts',
  'src/ts/storage/deviceStateRestore.test.ts',
  'src/ts/storage/effectiveIdentityState.test.ts',
  'src/ts/storage/exportAsDataset.test.ts',
  'src/ts/storage/exportExcludedReport.test.ts',
  'src/ts/storage/fileOperationErrorPresentation.test.ts',
  'src/ts/storage/hypaEmbeddingCache.test.ts',
  'src/ts/storage/imageGeometry.test.ts',
  'src/ts/storage/importedIdentity.test.ts',
  'src/ts/storage/iosContentPicker.test.ts',
  'src/ts/storage/libraryCharacters.test.ts',
  'src/ts/storage/libraryFileOperation.test.ts',
  'src/ts/storage/localDataRemotes.test.ts',
  'src/ts/storage/localDataSections.test.ts',
  'src/ts/storage/lwwSavePath.test.ts',
  'src/ts/storage/macosLifecycle.test.ts',
  'src/ts/storage/mutationGatedPersistentDataStore.test.ts',
  'src/ts/storage/nativeAccountBoundary.test.ts',
  'src/ts/storage/nativeAccountCredential.test.ts',
  'src/ts/storage/nativeAssetRepository.test.ts',
  'src/ts/storage/nativeCharacterCardExportRoute.test.ts',
  'src/ts/storage/nativeCharacterCharxExportRoute.test.ts',
  'src/ts/storage/nativeCharacterFileRoute.test.ts',
  'src/ts/storage/nativeCharacterImportOperation.test.ts',
  'src/ts/storage/nativeCharxModuleActivation.test.ts',
  'src/ts/storage/nativeContentFile.test.ts',
  'src/ts/storage/nativeDeviceSettings.test.ts',
  'src/ts/storage/nativeFileJobs.deviceStateRestore.test.ts',
  'src/ts/storage/nativeFileTaxonomyGolden.test.ts',
  'src/ts/storage/nativeJpegAssetImport.test.ts',
  'src/ts/storage/nativeMediaIpc.test.ts',
  'src/ts/storage/nativeModuleRisumExportRoute.test.ts',
  'src/ts/storage/nativeOfficialPublicationUpload.test.ts',
  'src/ts/storage/nativePaths.test.ts',
  'src/ts/storage/nativePersistenceValue.test.ts',
  'src/ts/storage/nativePersistentExport.test.ts',
  'src/ts/storage/nativePluginPermissions.test.ts',
  'src/ts/storage/nativePngCardAdapter.test.ts',
  'src/ts/storage/officialAccountMessage.test.ts',
  'src/ts/storage/ownerManifestCodec.test.ts',
  'src/ts/storage/payloadCas.test.ts',
  'src/ts/storage/persistenceBoundaries.integration.test.ts',
  'src/ts/storage/persistentConversationRead.test.ts',
  'src/ts/storage/persistentDataRuntime.committedApply.test.ts',
  'src/ts/storage/persistentDataRuntime.test.ts',
  'src/ts/storage/persistentDataRuntime.upstreamImport.test.ts',
  'src/ts/storage/persistentDataStoreFactory.test.ts',
  'src/ts/storage/persistentRecordIterator.test.ts',
  'src/ts/storage/persistentRevisionEvents.test.ts',
  'src/ts/storage/persistentRootFields.test.ts',
  'src/ts/storage/persistentSaveNotifications.test.ts',
  'src/ts/storage/persistentStorageRuntime.test.ts',
  'src/ts/storage/personaBindingPersistence.test.ts',
  'src/ts/storage/platformBlobStore.test.ts',
  'src/ts/storage/pluginStorageCaptureCache.test.ts',
  'src/ts/storage/pluginValueAssignmentRetention.test.ts',
  'src/ts/storage/portableBackupFileRouteProduction.test.ts',
  'src/ts/storage/preservationReportMessage.test.ts',
  'src/ts/storage/profileTransition.test.ts',
  'src/ts/storage/rawRecoveryExport.test.ts',
  'src/ts/storage/recoveryExclusionPrompt.test.ts',
  'src/ts/storage/remoteSaveCleanup.test.ts',
  'src/ts/storage/retainableReplacementFence.test.ts',
  'src/ts/storage/risuNestStorageDashboard.test.ts',
  'src/ts/storage/risuSaveFileRoute.test.ts',
  'src/ts/storage/rootMutation.test.ts',
  'src/ts/storage/saveCoordinator.explicitOperations.test.ts',
  'src/ts/storage/saveCoordinator.retiredRecord.test.ts',
  'src/ts/storage/saveCoordinator.reviewRegression.test.ts',
  'src/ts/storage/saveCoordinator.scopedScale.test.ts',
  'src/ts/storage/saveCoordinator.test.ts',
  'src/ts/storage/saveCoordinator.trashStamp.test.ts',
  'src/ts/storage/saveCoordinator.windowed.test.ts',
  'src/ts/storage/segmentedConversationResidency.test.ts',
  'src/ts/storage/selectedConversationLifecycle.test.ts',
  'src/ts/storage/storageMutationGate.test.ts',
  'src/ts/storage/streamingConversationRegistry.test.ts',
  'src/ts/storage/sync/bindingDefaults.test.ts',
  'src/ts/storage/sync/bindingDialog.test.ts',
  'src/ts/storage/sync/bindingFlow.test.ts',
  'src/ts/storage/sync/bindingNative.test.ts',
  'src/ts/storage/sync/bindingProduction.test.ts',
  'src/ts/storage/sync/bindingRegistry.test.ts',
  'src/ts/storage/sync/blockedReasonText.test.ts',
  'src/ts/storage/sync/external/applicationRecovery.test.ts',
  'src/ts/storage/sync/external/bridge.test.ts',
  'src/ts/storage/sync/external/connection.test.ts',
  'src/ts/storage/sync/external/lwwScheduler.test.ts',
  'src/ts/storage/sync/external/progress.test.ts',
  'src/ts/storage/sync/external/restoreScope.test.ts',
  'src/ts/storage/sync/external/scheduler.test.ts',
  'src/ts/storage/sync/logicalRecordKey.test.ts',
  'src/ts/storage/sync/nativeOfficialAccountAssets.test.ts',
  'src/ts/storage/sync/nativeOfficialAccountFlow.test.ts',
  'src/ts/storage/sync/officialAccountBootstrap.test.ts',
  'src/ts/storage/sync/officialAccountSnapshot.test.ts',
  'src/ts/storage/sync/officialAssetLedger.test.ts',
  'src/ts/storage/sync/remainingTime.test.ts',
  'src/ts/storage/sync/serverAssetResidency.test.ts',
  'src/ts/storage/sync/serverOnlyBoundary.test.ts',
  'src/ts/storage/sync/serverSyncDeepLink.test.ts',
  'src/ts/storage/sync/serverSyncNativeSignals.test.ts',
  'src/ts/storage/sync/serverSyncProgress.test.ts',
  'src/ts/storage/sync/serverSyncQr.test.ts',
  'src/ts/storage/sync/serverSyncRegistration.test.ts',
  'src/ts/storage/sync/serverSyncRegistrationDispatch.test.ts',
  'src/ts/storage/sync/serverSyncScheduler.test.ts',
  'src/ts/storage/sync/syncConflictBackup.test.ts',
  'src/ts/storage/sync/syncConflictRestore.test.ts',
  'src/ts/storage/sync/syncConflictSummary.test.ts',
  'src/ts/storage/sync/transferRate.test.ts',
  'src/ts/storage/syncExitCoordinator.test.ts',
  'src/ts/storage/syncExitProduction.test.ts',
  'src/ts/storage/targetedWorkingSetInvalidation.test.ts',
  'src/ts/storage/targetedWorkingSetRefresh.test.ts',
  'src/ts/storage/tests/activeWorkingSet.test.ts',
  'src/ts/storage/tests/conversationPatch.integration.test.ts',
  'src/ts/storage/tests/databasePreparation.test.ts',
  'src/ts/storage/tests/databaseRestore.test.ts',
  'src/ts/storage/tests/indexedDbPersistentDataStore.test.ts',
  'src/ts/storage/tests/nativeContractParity.test.ts',
  'src/ts/storage/tests/persistentBootstrap.test.ts',
  'src/ts/storage/tests/persistentFixtureExport.test.ts',
  'src/ts/storage/tests/persistentProductionIntegration.test.ts',
  'src/ts/storage/tests/roadmap14/adapters/frozenLegacyOracle.test.ts',
  'src/ts/storage/tests/roadmap14/adapters/msgpackrParity.test.ts',
  'src/ts/storage/tests/roadmap14/canonicalCompatibility.test.ts',
  'src/ts/storage/tests/roadmap14/losslessCorpus.test.ts',
  'src/ts/storage/tests/selectedConversationPromotion.integration.test.ts',
  'src/ts/storage/upstreamImport.test.ts',
  'src/ts/storage/upstreamReplacement.test.ts',
  'src/ts/storage/workingSetCatalog.test.ts',
  'src/ts/storage/workingSetResidency.test.ts',
  'src/ts/toggleBindings.test.ts',
  'src/ts/tokenizer/nativeTokenizer.test.ts',
  'src/ts/tokenizer/nativeTokenizerCorpus.test.ts',
  'src/ts/tokenizer/nativeTokenizerProduction.test.ts',
  'src/ts/translator/presets.test.ts',
  'src/ts/ui/chatScreenState.svelte.test.ts',
  'src/ts/ui/navigationActivity.test.ts',
  'src/ts/ui/qrScanner.test.ts',
  'src/ts/ui/specialDay.test.ts',
  'src/ts/update/manifest.test.ts',
  'src/ts/update/notes.test.ts',
  'src/ts/update/persistence.test.ts',
  'src/ts/util/tests/byteBudgetLru.test.ts',
  'tests/support/testNetwork.test.ts',
]
const nodeAppTestSet = new Set(nodeAppTests)
export const harnessVitestTests = [
  `benchmarks/${testSuffix}`,
  'src/ts/process/regexSafePlan.differential.test.ts',
  'src/ts/process/regexSafePlan.profile.test.ts',
]
export const nodeTestExceptions = [
  'crates/sync-wire/tests/golden.test.mjs',
  'benchmarks/sync-server/transfer-comparison.test.mjs',
]
export const separateRunnerPaths = ['tests/browser/**', 'tests/native/**', ...nodeTestExceptions]
export const ignoredDirectories = new Set([
  'node_modules', 'dist', '.git', '.tmp', '.worktrees', '.claude', '.superpowers',
  'target', 'build', '.gradle', '.svelte-kit', 'coverage', 'graft', 'docs',
])
export const testExcludes = [...ignoredDirectories].map(name => `**/${name}/**`)

export const normalizePath = path => path.replaceAll('\\', '/').replace(/^\.\//, '')
const ordinaryTest = /\.(?:test|spec)\.[cm]?[jt]sx?$/
const under = (path, root) => path.startsWith(`${root}/`)

export function isTestEntry(path) {
  path = normalizePath(path)
  return ordinaryTest.test(path) || path.endsWith('.node-test.mjs') || /(?:^|\/)tests\/.*\.worker\.ts$/.test(path)
}

export function ownerOf(path) {
  path = normalizePath(path)
  if (!isTestEntry(path)) return null
  if (nodeTestExceptions.includes(path)
    || (path.endsWith('.node-test.mjs') && ['tests', 'benchmarks'].some(root => under(path, root)))) {
    return under(path, 'benchmarks') ? 'harness-node' : 'node'
  }
  if (under(path, 'tests/browser')) return 'browser'
  if (under(path, 'tests/native')) return 'native'
  if (under(path, 'server/endpoint-registry/tests') && path.endsWith('.worker.ts')) return 'registry'
  if (under(path, 'server/manager/gui') && ordinaryTest.test(path)) return 'manager'
  if (under(path, 'tools/web-risu-export/tests') && ordinaryTest.test(path)) return 'web-export'
  if (extendedAppTests.includes(path)) return 'app-extended'
  if (nodeAppTestSet.has(path)) return 'app-node'
  if (ordinaryTest.test(path) && (under(path, 'benchmarks') || harnessVitestTests.includes(path))) return 'harness'
  if (ordinaryTest.test(path) && appRoots.some(root => under(path, root))) return 'app'
  throw new Error(`Unowned test entry: ${path}`)
}

// Walk independently of Vitest include patterns so new unowned roots cannot disappear silently.
export async function discoverTestEntries(root, relative = '') {
  const files = []
  for (const entry of await readdir(join(root, relative), { withFileTypes: true })) {
    if (entry.isSymbolicLink()) continue
    const path = relative ? `${relative}/${entry.name}` : entry.name
    if (entry.isDirectory() && !ignoredDirectories.has(entry.name)) {
      files.push(...await discoverTestEntries(root, path))
    } else if (entry.isFile() && isTestEntry(path)) {
      files.push(path)
    }
  }
  return files.sort()
}

export function selectNodeTests(files, owner = 'node') {
  const selected = files.map(normalizePath).filter(path => ownerOf(path) === owner).sort()
  if (selected.length === 0) throw new Error('No Node test entries found')
  if (new Set(selected).size !== selected.length) throw new Error('Duplicate Node test entries')
  return selected
}

// A listed path that no longer exists would silently stop routing a renamed test.
export function assertListedTestsExist(files) {
  const present = new Set(files.map(normalizePath))
  const missing = [...extendedAppTests, ...nodeAppTests, ...luaWorkerTests].filter(path => !present.has(path))
  if (missing.length) throw new Error(`Listed test entries missing: ${missing.join(', ')}`)
}

export function assertInventory(files, inventories) {
  const expected = new Map(files.map(path => [normalizePath(path), ownerOf(path)]).filter(([, owner]) => owner !== null))
  const repeated = luaWorkerTests.filter(path => expected.has(path))
  const worker = inventories['app-lua-worker'] ?? []
  if (new Set(worker).size !== worker.length || worker.length !== repeated.length
      || worker.some(path => !repeated.includes(path))) throw new Error('Lua Worker rerun inventory mismatch')
  const seen = new Set()
  for (const [owner, selected] of Object.entries(inventories)) {
    if (owner === 'app-lua-worker') continue
    for (const rawPath of selected) {
      const path = normalizePath(rawPath)
      if (seen.has(path)) throw new Error(`Duplicate test ownership: ${path}`)
      seen.add(path)
      if (expected.get(path) !== owner) throw new Error(`Unexpected ${owner} entry: ${path} (expected ${expected.get(path) ?? 'none'})`)
    }
  }
  for (const [path, owner] of expected) {
    if (!Object.hasOwn(inventories, owner)) throw new Error(`Missing runner inventory: ${owner} (${path})`)
    if (!seen.has(path)) throw new Error(`Runner omitted ${owner} test: ${path}`)
  }
}
