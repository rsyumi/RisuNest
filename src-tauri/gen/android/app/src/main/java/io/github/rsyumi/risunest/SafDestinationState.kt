package io.github.rsyumi.risunest

import java.io.File
import java.io.FileOutputStream
import java.util.concurrent.atomic.AtomicBoolean

private const val DESTINATION_STATE_VERSION = 3
private const val MAX_DESTINATION_STATE_BYTES = 8_192L
private const val MAX_DESTINATION_URI_CHARS = 2_048
internal const val DESTINATION_PICKER_STALE_MILLIS = 60 * 60 * 1_000L
private val SAFE_CODE = Regex("[a-z0-9-]{1,64}")

internal enum class SafDestinationPhase(val wireName: String) {
  PICKING("picking"),
  COPYING("copying"),
  CANCELLING("cancelling"),
  SUCCEEDED("succeeded"),
  FAILED("failed"),
  CANCELLED("cancelled");

  companion object {
    fun fromWireName(value: String): SafDestinationPhase? = entries.find { it.wireName == value }
  }
}

internal enum class SafDestinationSourceKind(val wireName: String) {
  RISU_SAVE("risuSave"),
  LEGACY_BACKUP("legacyBackup"),
  SCREENSHOT("screenshot");

  companion object {
    fun fromWireName(value: String): SafDestinationSourceKind? =
      entries.find { it.wireName == value }
  }
}

internal enum class SafDestinationRecoveryAction {
  WAIT_FOR_PICKER,
  CLEAN_PARTIAL,
  FAIL_INTERRUPTED,
  REPLAY_TERMINAL,
}

internal class SafDestinationSlot {
  private val held = AtomicBoolean(false)

  fun tryAcquire(): Boolean = held.compareAndSet(false, true)

  fun acquireRestored() {
    held.set(true)
  }

  fun release() {
    held.set(false)
  }
}

internal fun isPendingSafDestinationPicker(record: SafDestinationRecord): Boolean =
  record.destinationUri == null &&
    record.phase in setOf(SafDestinationPhase.PICKING, SafDestinationPhase.CANCELLING)

internal fun selectedSafDestinationState(
  record: SafDestinationRecord,
  requestId: String,
  destinationUri: String?,
  cancellationRequested: Boolean,
  nowMillis: Long,
): SafDestinationRecord? {
  if (record.requestId != requestId || !isPendingSafDestinationPicker(record)) return null
  if (destinationUri == null) {
    return record.copy(
      phase = SafDestinationPhase.CANCELLED,
      destinationUri = null,
      bytes = null,
      code = "cancelled",
      warningCodes = emptyList(),
      updatedAtMillis = nowMillis,
    )
  }
  return record.copy(
    phase = if (record.phase == SafDestinationPhase.CANCELLING || cancellationRequested) {
      SafDestinationPhase.CANCELLING
    } else {
      SafDestinationPhase.COPYING
    },
    destinationUri = destinationUri,
    bytes = null,
    code = null,
    warningCodes = listOf("android-saf-provider-not-atomic"),
    updatedAtMillis = nowMillis,
  )
}

internal fun expiredSafDestinationState(
  record: SafDestinationRecord,
  requestId: String,
  nowMillis: Long,
): SafDestinationRecord? {
  if (
    record.requestId != requestId ||
    !isPendingSafDestinationPicker(record) ||
    nowMillis - record.updatedAtMillis < DESTINATION_PICKER_STALE_MILLIS
  ) return null
  val wasCancelling = record.phase == SafDestinationPhase.CANCELLING
  return record.copy(
    phase = if (wasCancelling) SafDestinationPhase.CANCELLED else SafDestinationPhase.FAILED,
    destinationUri = null,
    bytes = null,
    code = if (wasCancelling) "cancelled" else "destination-interrupted",
    warningCodes = emptyList(),
    updatedAtMillis = nowMillis,
  )
}

internal fun decideSafDestinationRecovery(
  record: SafDestinationRecord,
  hasRestoredActivityState: Boolean,
  nowMillis: Long = System.currentTimeMillis(),
): SafDestinationRecoveryAction = when (record.phase) {
  SafDestinationPhase.PICKING -> if (
    hasRestoredActivityState &&
    nowMillis - record.updatedAtMillis < DESTINATION_PICKER_STALE_MILLIS
  ) {
    SafDestinationRecoveryAction.WAIT_FOR_PICKER
  } else {
    SafDestinationRecoveryAction.FAIL_INTERRUPTED
  }
  SafDestinationPhase.COPYING -> SafDestinationRecoveryAction.CLEAN_PARTIAL
  SafDestinationPhase.CANCELLING -> if (record.destinationUri == null) {
    if (
      hasRestoredActivityState &&
      nowMillis - record.updatedAtMillis < DESTINATION_PICKER_STALE_MILLIS
    ) {
      SafDestinationRecoveryAction.WAIT_FOR_PICKER
    } else {
      SafDestinationRecoveryAction.FAIL_INTERRUPTED
    }
  } else {
    SafDestinationRecoveryAction.CLEAN_PARTIAL
  }
  SafDestinationPhase.SUCCEEDED,
  SafDestinationPhase.FAILED,
  SafDestinationPhase.CANCELLED,
  -> SafDestinationRecoveryAction.REPLAY_TERMINAL
}

internal data class SafDestinationRecord(
  val requestId: String,
  val exportId: String,
  val phase: SafDestinationPhase,
  val destinationUri: String?,
  val bytes: Long?,
  val code: String?,
  val warningCodes: List<String>,
  val updatedAtMillis: Long,
  val sourceKind: SafDestinationSourceKind = SafDestinationSourceKind.RISU_SAVE,
  val publicationPrerequisitesComplete: Boolean = false,
) {
  fun isTerminal() = phase in setOf(
    SafDestinationPhase.SUCCEEDED,
    SafDestinationPhase.FAILED,
    SafDestinationPhase.CANCELLED,
  )
}

internal class SafDestinationStateStore(
  private val stateFile: File,
  private val atomicPublisher: SafAtomicPublisher,
) {
  fun load(): SafDestinationRecord? {
    if (!stateFile.isFile || stateFile.length() > MAX_DESTINATION_STATE_BYTES) return null
    val json = runCatching { stateFile.readText(Charsets.UTF_8) }.getOrNull() ?: return null
    val version = numberField(json, "version") ?: return null
    if (version !in 1L..DESTINATION_STATE_VERSION.toLong()) return null
    val requestId = stringField(json, "requestId") ?: return null
    val exportId = stringField(json, "exportId") ?: return null
    val phase = stringField(json, "phase")?.let(SafDestinationPhase::fromWireName) ?: return null
    val destinationUri = nullableStringField(json, "destinationUri") ?: return null
    val bytes = nullableLongField(json, "bytes") ?: return null
    val code = nullableStringField(json, "code") ?: return null
    val warningCodes = warningCodesField(json) ?: return null
    val updatedAtMillis = numberField(json, "updatedAtMillis") ?: return null
    val sourceKind = if (version == 1L) {
      SafDestinationSourceKind.RISU_SAVE
    } else {
      stringField(json, "sourceKind")?.let(SafDestinationSourceKind::fromWireName) ?: return null
    }
    val publicationPrerequisitesComplete = if (version < 3L) {
      false
    } else {
      booleanField(json, "publicationPrerequisitesComplete") ?: return null
    }
    val record = SafDestinationRecord(
      requestId = requestId,
      exportId = exportId,
      phase = phase,
      destinationUri = destinationUri.value,
      bytes = bytes.value,
      code = code.value,
      warningCodes = warningCodes,
      updatedAtMillis = updatedAtMillis,
      sourceKind = sourceKind,
      publicationPrerequisitesComplete = publicationPrerequisitesComplete,
    )
    return record.takeIf(::isValidRecord)
  }

  fun save(record: SafDestinationRecord) {
    require(isValidRecord(record)) { "invalid Android SAF destination state" }
    val parent = stateFile.parentFile ?: error("destination state requires a parent")
    check(parent.mkdirs() || parent.isDirectory) { "destination state root cannot be created" }
    val temporary = parent.resolve("${stateFile.name}.tmp")
    parent.resolve("${stateFile.name}.cleared").delete()
    val warnings = record.warningCodes.joinToString(",") { "\"$it\"" }
    val json = "{" +
      "\"version\":$DESTINATION_STATE_VERSION," +
      "\"requestId\":\"${record.requestId}\"," +
      "\"exportId\":\"${record.exportId}\"," +
      "\"sourceKind\":\"${record.sourceKind.wireName}\"," +
      "\"phase\":\"${record.phase.wireName}\"," +
      "\"destinationUri\":${record.destinationUri?.let { "\"$it\"" } ?: "null"}," +
      "\"bytes\":${record.bytes ?: "null"}," +
      "\"code\":${record.code?.let { "\"$it\"" } ?: "null"}," +
      "\"warningCodes\":[$warnings]," +
      "\"publicationPrerequisitesComplete\":${record.publicationPrerequisitesComplete}," +
      "\"updatedAtMillis\":${record.updatedAtMillis}" +
      "}"
    try {
      FileOutputStream(temporary, false).use { output ->
        output.write(json.toByteArray(Charsets.UTF_8))
        output.flush()
        output.fd.sync()
      }
      atomicPublisher.publish(temporary, stateFile)
    } catch (error: Exception) {
      temporary.delete()
      throw error
    }
  }

  fun clear(requestId: String): Boolean {
    val record = load() ?: return false
    if (record.requestId != requestId || !record.isTerminal()) return false
    val parent = stateFile.parentFile ?: return false
    parent.resolve("${stateFile.name}.tmp").delete()
    val cleared = parent.resolve("${stateFile.name}.cleared")
    cleared.delete()
    return try {
      atomicPublisher.publish(stateFile, cleared)
      cleared.delete()
      true
    } catch (error: Exception) {
      !stateFile.exists()
    }
  }
}

internal fun completedSafPublicationPrerequisites(
  record: SafDestinationRecord,
  requestId: String,
  nowMillis: Long,
): SafDestinationRecord? {
  if (
    record.requestId != requestId ||
    record.sourceKind != SafDestinationSourceKind.RISU_SAVE ||
    !record.isTerminal()
  ) return null
  return record.copy(
    publicationPrerequisitesComplete = true,
    updatedAtMillis = nowMillis,
  )
}

internal fun acknowledgeSafDestinationExport(
  requestId: String,
  load: () -> SafDestinationRecord?,
  requiresPublicationProof: (SafDestinationRecord) -> Boolean,
  prepare: (SafDestinationRecord) -> Boolean,
  clear: (String) -> Boolean,
): Boolean {
  if (!isCanonicalUuidV4(requestId)) return false
  val record = load()
    ?.takeIf { it.requestId == requestId && it.isTerminal() }
    ?: return false
  if (
    requiresPublicationProof(record) &&
    !record.publicationPrerequisitesComplete
  ) return false
  if (!prepare(record)) return false
  return clear(requestId)
}

internal fun interruptedSafDestinationWarnings(deletePartial: () -> Boolean): List<String> {
  val warnings = mutableListOf("android-saf-provider-not-atomic")
  if (!runCatching(deletePartial).getOrDefault(false)) {
    warnings.add("partial-destination-may-remain")
  }
  return warnings
}

private data class NullableString(val value: String?)
private data class NullableLong(val value: Long?)

private fun isValidRecord(record: SafDestinationRecord): Boolean {
  if (!isCanonicalUuidV4(record.requestId) || !isCanonicalUuidV4(record.exportId)) return false
  if (record.updatedAtMillis < 0 || record.bytes?.let { it < 0 } == true) return false
  if (record.code?.let { !SAFE_CODE.matches(it) } == true) return false
  if (record.warningCodes.size > 16 || record.warningCodes.any { !SAFE_CODE.matches(it) }) return false
  val uri = record.destinationUri
  if (
    uri != null &&
    (
      uri.length > MAX_DESTINATION_URI_CHARS ||
        !uri.startsWith("content://") ||
        uri.any { it == '"' || it == '\\' || it < ' ' }
      )
  ) return false
  if (record.phase == SafDestinationPhase.PICKING && uri != null) return false
  if (
    record.phase == SafDestinationPhase.COPYING &&
    uri == null
  ) return false
  return true
}

// Shared with SafFileBridge.kt's spool manifest parsing.
internal fun stringField(json: String, name: String): String? =
  Regex("\\\"$name\\\":\\\"([^\\\"]*)\\\"").find(json)?.groupValues?.get(1)

private fun nullableStringField(json: String, name: String): NullableString? {
  val match = Regex("\\\"$name\\\":(null|\\\"([^\\\"]*)\\\")").find(json) ?: return null
  return NullableString(if (match.groupValues[1] == "null") null else match.groupValues[2])
}

internal fun numberField(json: String, name: String): Long? =
  Regex("\\\"$name\\\":([0-9]+)").find(json)?.groupValues?.get(1)?.toLongOrNull()

private fun booleanField(json: String, name: String): Boolean? =
  Regex("\\\"$name\\\":(true|false)").find(json)?.groupValues?.get(1)?.toBooleanStrictOrNull()

private fun nullableLongField(json: String, name: String): NullableLong? {
  val match = Regex("\\\"$name\\\":(null|[0-9]+)").find(json) ?: return null
  if (match.groupValues[1] == "null") return NullableLong(null)
  return match.groupValues[1].toLongOrNull()?.let(::NullableLong)
}

private fun warningCodesField(json: String): List<String>? {
  val body = Regex("\\\"warningCodes\\\":\\[([^]]*)]").find(json)?.groupValues?.get(1)
    ?: return null
  if (body.isEmpty()) return emptyList()
  return body.split(',').map { encoded ->
    Regex("\\\"([a-z0-9-]{1,64})\\\"").matchEntire(encoded)?.groupValues?.get(1)
      ?: return null
  }
}
