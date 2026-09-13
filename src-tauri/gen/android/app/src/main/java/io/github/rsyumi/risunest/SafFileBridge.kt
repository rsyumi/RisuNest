package io.github.rsyumi.risunest

import android.system.Os
import android.system.OsConstants
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.UUID
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

private const val DEFAULT_COPY_BUFFER_BYTES = 64 * 1024
private const val DEFAULT_STALE_AFTER_MILLIS = 24 * 60 * 60 * 1_000L
private const val MAX_DISPLAY_NAME_CHARS = 180
private const val SPOOL_OWNERSHIP_FORMAT = "risunest-android-saf-spool"
private const val SPOOL_STAGING_PREFIX = ".spooling-"
private const val SPOOL_CLEANUP_PREFIX = ".cleanup-"
private val BACKUP_SOURCE_SUFFIXES = listOf(
  ".risunest",
  ".risudat",
  ".bin",
)
private val NATIVE_FILE_JOB_SPOOL_SUFFIXES = BACKUP_SOURCE_SUFFIXES + listOf(
  ".charx",
  ".json",
  ".jpeg",
  ".jpg",
  ".png",
  ".risum",
)
private val CANONICAL_TOKEN = Regex(
  "[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
)

internal fun isCanonicalUuidV4(value: String): Boolean = CANONICAL_TOKEN.matches(value)

internal fun isBackupSource(displayName: String): Boolean =
  BACKUP_SOURCE_SUFFIXES.any { suffix ->
    displayName.endsWith(suffix, ignoreCase = true)
  }

internal fun shouldUseNativeFileJobSpool(displayName: String): Boolean =
  NATIVE_FILE_JOB_SPOOL_SUFFIXES.any { suffix ->
    displayName.endsWith(suffix, ignoreCase = true)
  }

private val MANAGED_EXPORT_NAME = Regex(
  "risusave-([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})\\.risudat",
)
private val MANAGED_LEGACY_BACKUP_NAME = Regex(
  "risu-backup-([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})\\.bin",
)
private val MANAGED_PORTABLE_BACKUP_NAME = Regex(
  "risunest-backup-([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})\\.risunest",
)
private val MANAGED_CHARACTER_CHARX_NAME = Regex(
  "risu-charx-([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})\\.(?:charx|jpeg)",
)
private val MANAGED_CHARACTER_CARD_NAME = Regex(
  "risu-character-card-([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})\\.(?:json|png)",
)
private val MANAGED_RISU_MODULE_NAME = Regex(
  "risu-module-([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})\\.risum",
)
private const val MANAGED_SCREENSHOT_FILE = "archive.zip.part"
private const val MANAGED_SCREENSHOT_OWNERSHIP = "ownership"
private const val MANAGED_SCREENSHOT_READY = "ready"

internal interface SafInputSource {
  val displayName: String
  val totalBytes: Long?
  fun open(): InputStream
}

internal data class SafSpoolProgress(
  val token: String,
  val copiedBytes: Long,
  val totalBytes: Long?,
)

internal data class SafSpoolReady(
  val token: String,
  val displayName: String,
  val bytes: Long,
  val totalBytes: Long?,
)

internal data class SafSpoolFailure(
  val displayName: String,
  val code: String,
)

internal data class SafSpoolBatch(
  val ready: List<SafSpoolReady>,
  val failures: List<SafSpoolFailure>,
)

internal fun interface SafAtomicPublisher {
  fun publish(temporary: File, target: File)
}

internal object AndroidSafAtomicPublisher : SafAtomicPublisher {
  override fun publish(temporary: File, target: File) {
    Os.rename(temporary.absolutePath, target.absolutePath)
    val directory = Os.open(
      target.parentFile!!.absolutePath,
      OsConstants.O_RDONLY,
      0,
    )
    try {
      Os.fsync(directory)
    } finally {
      Os.close(directory)
    }
  }
}

private data class SafSpoolOwnership(
  val token: String,
  val createdAtMillis: Long,
)

internal class SafSpoolStore(
  root: File,
  private val bufferBytes: Int = DEFAULT_COPY_BUFFER_BYTES,
  private val atomicPublisher: SafAtomicPublisher,
  private val nowMillis: () -> Long = System::currentTimeMillis,
  private val tokenFactory: () -> UUID = UUID::randomUUID,
) {
  private val root = root.absoluteFile

  init {
    require(bufferBytes > 0) { "SAF copy buffer must be positive" }
  }

  fun spool(
    sources: List<SafInputSource>,
    isCancelled: () -> Boolean = { false },
    onProgress: (SafSpoolProgress) -> Unit = {},
  ): SafSpoolBatch {
    root.mkdirs()
    val ready = mutableListOf<SafSpoolReady>()
    val failures = mutableListOf<SafSpoolFailure>()
    for (source in sources) {
      val displayName = safeSafDisplayName(source.displayName)
      val token = tokenFactory().toString()
      if (!CANONICAL_TOKEN.matches(token)) {
        failures.add(SafSpoolFailure(displayName, "invalid-token"))
        continue
      }
      val ownedDirectory = root.resolve(token)
      val stagingDirectory = root.resolve("$SPOOL_STAGING_PREFIX$token")
      try {
        if (!stagingDirectory.mkdir()) {
          throw SafSpoolException("spool-create-failed", "SAF spool directory cannot be created")
        }
        writeOwnership(stagingDirectory, token, nowMillis())
        writeManifest(
          stagingDirectory,
          token,
          "copying",
          displayName,
          bytes = null,
          totalBytes = source.totalBytes,
        )
        val copiedBytes = copySource(
          source,
          stagingDirectory.resolve("source.risudat"),
          token,
          isCancelled,
          onProgress,
        )
        writeManifest(
          stagingDirectory,
          token,
          "ready",
          displayName,
          bytes = copiedBytes,
          totalBytes = source.totalBytes,
        )
        atomicPublisher.publish(stagingDirectory, ownedDirectory)
        ready.add(SafSpoolReady(token, displayName, copiedBytes, source.totalBytes))
      } catch (error: SafSpoolException) {
        deleteGeneratedDirectory(stagingDirectory, token)
        deleteGeneratedDirectory(ownedDirectory, token)
        failures.add(SafSpoolFailure(displayName, error.code))
      } catch (error: Exception) {
        deleteGeneratedDirectory(stagingDirectory, token)
        deleteGeneratedDirectory(ownedDirectory, token)
        failures.add(SafSpoolFailure(displayName, "spool-write-failed"))
      }
    }
    return SafSpoolBatch(ready, failures)
  }

  fun cleanupStale(
    nowMillis: Long = System.currentTimeMillis(),
    staleAfterMillis: Long = DEFAULT_STALE_AFTER_MILLIS,
    activeTokens: Set<String> = emptySet(),
  ): List<String> {
    if (!root.isDirectory) return emptyList()
    val canonicalRoot = runCatching { root.canonicalFile }.getOrNull() ?: return emptyList()
    val removed = mutableListOf<String>()
    for (candidate in root.listFiles().orEmpty()) {
      val token = candidate.name.removePrefix(SPOOL_CLEANUP_PREFIX)
      if (
        candidate.name == token ||
        !candidate.isDirectory ||
        !CANONICAL_TOKEN.matches(token)
      ) continue
      val ownership = runCatching {
        readSpoolOwnership(candidate.resolve("ownership.json"))
      }.getOrNull()
      if (ownership?.token == token) deleteGeneratedDirectory(candidate, token)
    }
    for (candidate in root.listFiles().orEmpty()) {
      val isStaging = candidate.name.startsWith(SPOOL_STAGING_PREFIX)
      val token = if (isStaging) {
        candidate.name.removePrefix(SPOOL_STAGING_PREFIX)
      } else {
        candidate.name
      }
      if (!candidate.isDirectory || !CANONICAL_TOKEN.matches(token) || token in activeTokens) continue
      val canonicalCandidate = runCatching { candidate.canonicalFile }.getOrNull() ?: continue
      if (canonicalCandidate.parentFile != canonicalRoot) continue
      val ownership = runCatching {
        readSpoolOwnership(candidate.resolve("ownership.json"))
      }.getOrNull()
      val createdAtMillis = ownership?.createdAtMillis ?: candidate.lastModified()
      if ((!isStaging && ownership == null) || ownership?.token?.let { it != token } == true) continue
      if (nowMillis - createdAtMillis < staleAfterMillis) continue
      val cleanupDirectory = root.resolve("$SPOOL_CLEANUP_PREFIX$token")
      if (cleanupDirectory.exists()) {
        val cleanupOwnership = runCatching {
          readSpoolOwnership(cleanupDirectory.resolve("ownership.json"))
        }.getOrNull()
        if (
          cleanupOwnership?.token != token ||
          !deleteGeneratedDirectory(cleanupDirectory, token)
        ) continue
      }
      try {
        atomicPublisher.publish(candidate, cleanupDirectory)
      } catch (error: Exception) {
        continue
      }
      if (deleteGeneratedDirectory(cleanupDirectory, token)) removed.add(token)
    }
    return removed.distinct().sorted()
  }

  fun listReady(): List<SafSpoolReady> {
    if (!root.isDirectory) return emptyList()
    val canonicalRoot = runCatching { root.canonicalFile }.getOrNull() ?: return emptyList()
    return root.listFiles().orEmpty().mapNotNull { candidate ->
      val token = candidate.name
      if (!candidate.isDirectory || !CANONICAL_TOKEN.matches(token)) return@mapNotNull null
      val canonicalCandidate = runCatching { candidate.canonicalFile }.getOrNull()
        ?: return@mapNotNull null
      if (canonicalCandidate.parentFile != canonicalRoot) return@mapNotNull null
      val ownership = runCatching {
        readSpoolOwnership(candidate.resolve("ownership.json"))
      }.getOrNull() ?: return@mapNotNull null
      if (ownership.token != token) return@mapNotNull null
      readReadySpool(candidate, token)
    }.sortedBy(SafSpoolReady::token)
  }

  fun discardReady(token: String): Boolean {
    if (!isCanonicalUuidV4(token) || !root.isDirectory) return false
    val canonicalRoot = runCatching { root.canonicalFile }.getOrNull() ?: return false
    val ownedDirectory = root.resolve(token)
    val canonicalOwned = runCatching { ownedDirectory.canonicalFile }.getOrNull() ?: return false
    if (!ownedDirectory.isDirectory || canonicalOwned.parentFile != canonicalRoot) return false
    val ownership = runCatching {
      readSpoolOwnership(ownedDirectory.resolve("ownership.json"))
    }.getOrNull() ?: return false
    if (ownership.token != token || readReadySpool(ownedDirectory, token) == null) return false
    val cleanupDirectory = root.resolve("$SPOOL_CLEANUP_PREFIX$token")
    if (cleanupDirectory.exists()) return false
    return try {
      atomicPublisher.publish(ownedDirectory, cleanupDirectory)
      deleteGeneratedDirectory(cleanupDirectory, token)
    } catch (error: Exception) {
      false
    }
  }

  private fun copySource(
    source: SafInputSource,
    target: File,
    token: String,
    isCancelled: () -> Boolean,
    onProgress: (SafSpoolProgress) -> Unit,
  ): Long {
    val input = try {
      source.open()
    } catch (error: Exception) {
      throw SafSpoolException("source-open-failed", "SAF source cannot be opened", error)
    }
    var copiedBytes = 0L
    try {
      input.use { openedInput ->
        FileOutputStream(target).use { output ->
          val buffer = ByteArray(bufferBytes)
          while (true) {
            checkCancellation(isCancelled)
            val bytesRead = try {
              openedInput.read(buffer)
            } catch (error: Exception) {
              throw SafSpoolException("source-read-failed", "SAF source cannot be read", error)
            }
            if (bytesRead < 0) break
            if (bytesRead == 0) continue
            checkCancellation(isCancelled)
            try {
              output.write(buffer, 0, bytesRead)
            } catch (error: Exception) {
              throw SafSpoolException("spool-write-failed", "SAF spool cannot be written", error)
            }
            copiedBytes += bytesRead
            onProgress(SafSpoolProgress(token, copiedBytes, source.totalBytes))
            checkCancellation(isCancelled)
          }
          try {
            output.flush()
            output.fd.sync()
          } catch (error: Exception) {
            throw SafSpoolException("spool-write-failed", "SAF spool cannot be flushed", error)
          }
        }
      }
    } catch (error: SafSpoolException) {
      throw error
    } catch (error: Exception) {
      throw SafSpoolException("source-read-failed", "SAF source cannot be closed", error)
    }
    return copiedBytes
  }

  private fun writeManifest(
    directory: File,
    token: String,
    state: String,
    displayName: String,
    bytes: Long?,
    totalBytes: Long?,
  ) {
    val json = "{" +
      "\"token\":${jsonString(token)}," +
      "\"state\":${jsonString(state)}," +
      "\"displayName\":${jsonString(displayName)}," +
      "\"bytes\":${bytes ?: "null"}," +
      "\"totalBytes\":${totalBytes ?: "null"}" +
      "}"
    writeDurableJson(directory, "source.json", json, "SAF spool manifest")
  }

  private fun writeOwnership(directory: File, token: String, createdAtMillis: Long) {
    val json = "{" +
      "\"format\":${jsonString(SPOOL_OWNERSHIP_FORMAT)}," +
      "\"version\":1," +
      "\"token\":${jsonString(token)}," +
      "\"createdAtMillis\":$createdAtMillis" +
      "}"
    writeDurableJson(directory, "ownership.json", json, "SAF spool ownership")
  }

  private fun writeDurableJson(directory: File, name: String, json: String, label: String) {
    val target = directory.resolve(name)
    val temporary = directory.resolve("$name.tmp")
    try {
      FileOutputStream(temporary, false).use { output ->
        output.write(json.toByteArray(Charsets.UTF_8))
        output.flush()
        output.fd.sync()
      }
      atomicPublisher.publish(temporary, target)
    } catch (error: Exception) {
      temporary.delete()
      throw SafSpoolException("spool-write-failed", "$label cannot be written", error)
    }
  }

  private fun deleteGeneratedDirectory(directory: File, token: String): Boolean {
    if (!CANONICAL_TOKEN.matches(token)) return false
    val canonicalRoot = runCatching { root.canonicalFile }.getOrNull() ?: return false
    val canonicalDirectory = runCatching { directory.canonicalFile }.getOrNull() ?: return false
    val allowedNames = setOf(
      token,
      "$SPOOL_STAGING_PREFIX$token",
      "$SPOOL_CLEANUP_PREFIX$token",
    )
    if (
      canonicalDirectory.parentFile != canonicalRoot ||
      canonicalDirectory.name !in allowedNames
    ) return false
    var deleted = true
    for (name in listOf(
      "source.risudat",
      "source.json.tmp",
      "source.json",
      "ownership.json.tmp",
      "ownership.json",
      "claim.lock",
    )) {
      val file = canonicalDirectory.resolve(name)
      if (file.exists() && !file.delete()) deleted = false
    }
    return canonicalDirectory.delete() && deleted
  }
}

internal suspend fun spoolOpenedFilesOnIo(
  store: SafSpoolStore,
  sources: List<SafInputSource>,
  isCancelled: () -> Boolean = { false },
  onProgress: (SafSpoolProgress) -> Unit = {},
): SafSpoolBatch = withContext(Dispatchers.IO) {
  store.spool(sources, isCancelled, onProgress)
}

internal data class SafDestinationResult(
  val bytes: Long,
  val warningCodes: List<String>,
)

internal class SafDestinationException(
  val code: String,
  val warningCodes: List<String>,
  message: String,
  cause: Throwable? = null,
) : IOException(message, cause)

internal suspend fun copySafDestinationOnIo(
  source: File,
  openSource: () -> InputStream = { source.inputStream() },
  openDestination: () -> OutputStream,
  deletePartial: () -> Boolean,
  createdDocument: Boolean,
  isCancelled: () -> Boolean = { false },
  onProgress: (Long) -> Unit = {},
  bufferBytes: Int = DEFAULT_COPY_BUFFER_BYTES,
): SafDestinationResult = withContext(Dispatchers.IO) {
  require(bufferBytes > 0) { "SAF copy buffer must be positive" }
  val baseWarnings = listOf("android-saf-provider-not-atomic")
  var copiedBytes = 0L
  try {
    openSource().use { input ->
      openDestination().use { output ->
        val buffer = ByteArray(bufferBytes)
        while (true) {
          checkDestinationCancellation(isCancelled, baseWarnings)
          val bytesRead = input.read(buffer)
          if (bytesRead < 0) break
          if (bytesRead == 0) continue
          checkDestinationCancellation(isCancelled, baseWarnings)
          output.write(buffer, 0, bytesRead)
          copiedBytes += bytesRead
          onProgress(copiedBytes)
          checkDestinationCancellation(isCancelled, baseWarnings)
        }
        output.flush()
      }
    }
    SafDestinationResult(copiedBytes, baseWarnings)
  } catch (error: SafDestinationException) {
    throw withPartialCleanup(error, deletePartial, createdDocument)
  } catch (error: Exception) {
    throw withPartialCleanup(
      SafDestinationException(
        "destination-write-failed",
        baseWarnings,
        "Android SAF destination copy failed",
        error,
      ),
      deletePartial,
      createdDocument,
    )
  }
}

// Every managed handoff kind shares one acceptance rule: a regular file whose
// canonical parent is exactly native-file-jobs/handoffs and whose full name
// matches the kind's regex. RisuSave (lease validation under persistent/exports)
// and screenshot (ownership/ready markers) stay special cases below.
private class ManagedHandoffKind(
  val nameRegex: Regex,
  val sourceKind: SafDestinationSourceKind,
  val fileNamesFor: (String) -> List<String>,
)

private val MANAGED_HANDOFF_KINDS = listOf(
  ManagedHandoffKind(MANAGED_PORTABLE_BACKUP_NAME, SafDestinationSourceKind.RISU_SAVE) { id ->
    listOf("risunest-backup-$id.risunest")
  },
  ManagedHandoffKind(MANAGED_LEGACY_BACKUP_NAME, SafDestinationSourceKind.LEGACY_BACKUP) { id ->
    listOf("risu-backup-$id.bin")
  },
  ManagedHandoffKind(MANAGED_CHARACTER_CHARX_NAME, SafDestinationSourceKind.RISU_SAVE) { id ->
    listOf("risu-charx-$id.charx", "risu-charx-$id.jpeg")
  },
  ManagedHandoffKind(MANAGED_CHARACTER_CARD_NAME, SafDestinationSourceKind.RISU_SAVE) { id ->
    listOf("risu-character-card-$id.json", "risu-character-card-$id.png")
  },
  ManagedHandoffKind(MANAGED_RISU_MODULE_NAME, SafDestinationSourceKind.RISU_SAVE) { id ->
    listOf("risu-module-$id.risum")
  },
)

internal fun resolveManagedExportSource(appDataRoot: File, sourcePath: String): File? {
  resolveManagedRisuSaveSource(appDataRoot, sourcePath)?.let { return it }
  for (kind in MANAGED_HANDOFF_KINDS) {
    resolveManagedHandoffSource(appDataRoot, sourcePath, kind.nameRegex)?.let { return it }
  }
  return resolveManagedScreenshotSource(appDataRoot, sourcePath)
}

private fun resolveManagedHandoffSource(
  appDataRoot: File,
  sourcePath: String,
  nameRegex: Regex,
): File? {
  val handoffsRoot = runCatching {
    appDataRoot.resolve("native-file-jobs/handoffs").canonicalFile
  }.getOrNull() ?: return null
  if (!handoffsRoot.isDirectory) return null
  val source = runCatching { File(sourcePath).canonicalFile }.getOrNull() ?: return null
  if (!source.isFile || source.parentFile != handoffsRoot) return null
  if (nameRegex.matchEntire(source.name) == null) return null
  return source
}

private fun resolveManagedRisuSaveSource(appDataRoot: File, sourcePath: String): File? {
  val exportsRoot = runCatching {
    appDataRoot.resolve("persistent/exports").canonicalFile
  }.getOrNull() ?: return null
  if (!exportsRoot.isDirectory) return null
  val source = runCatching { File(sourcePath).canonicalFile }.getOrNull() ?: return null
  if (!source.isFile || source.parentFile != exportsRoot) return null
  val match = MANAGED_EXPORT_NAME.matchEntire(source.name) ?: return null
  val id = match.groupValues[1]
  val ownership = exportsRoot.resolve("risusave-$id.lease")
  if (!ownership.isFile || ownership.length() > 4_096) return null
  val manifestId = Regex("\\\"exportId\\\":\\\"([^\\\"]+)\\\"")
    .find(runCatching { ownership.readText(Charsets.UTF_8) }.getOrNull() ?: return null)
    ?.groupValues
    ?.get(1)
  if (manifestId != id) return null
  return source
}

private fun resolveManagedScreenshotSource(appDataRoot: File, sourcePath: String): File? {
  val screenshotRoot = runCatching {
    appDataRoot.resolve("native-file-jobs/screenshot-output").canonicalFile
  }.getOrNull() ?: return null
  if (!screenshotRoot.isDirectory) return null
  val source = runCatching { File(sourcePath).canonicalFile }.getOrNull() ?: return null
  if (!source.isFile || source.name != MANAGED_SCREENSHOT_FILE) return null
  val directory = source.parentFile ?: return null
  if (directory.parentFile != screenshotRoot || !isCanonicalUuidV4(directory.name)) return null
  if (readExactOwner(directory.resolve(MANAGED_SCREENSHOT_OWNERSHIP)) != directory.name) return null
  if (readExactOwner(directory.resolve(MANAGED_SCREENSHOT_READY)) != directory.name) return null
  return source
}

private fun readExactOwner(marker: File): String? {
  if (!marker.isFile || marker.length() > 64) return null
  return runCatching { marker.readText(Charsets.UTF_8) }.getOrNull()
}

internal fun managedExportId(source: File): String? {
  MANAGED_EXPORT_NAME.matchEntire(source.name)?.groupValues?.get(1)?.let { return it }
  for (kind in MANAGED_HANDOFF_KINDS) {
    kind.nameRegex.matchEntire(source.name)?.groupValues?.get(1)?.let { return it }
  }
  if (source.name != MANAGED_SCREENSHOT_FILE) return null
  return source.parentFile?.name?.takeIf(::isCanonicalUuidV4)
}

internal fun managedExportSourceKind(source: File): SafDestinationSourceKind = when {
  source.name == MANAGED_SCREENSHOT_FILE -> SafDestinationSourceKind.SCREENSHOT
  else -> MANAGED_HANDOFF_KINDS.firstOrNull { it.nameRegex.matches(source.name) }?.sourceKind
    ?: SafDestinationSourceKind.RISU_SAVE
}

internal fun resolveManagedExportById(appDataRoot: File, exportId: String): File? {
  if (!isCanonicalUuidV4(exportId)) return null
  val source = appDataRoot.resolve("persistent/exports/risusave-$exportId.risudat")
  resolveManagedRisuSaveSource(appDataRoot, source.absolutePath)?.let { return it }
  for (kind in MANAGED_HANDOFF_KINDS) {
    for (fileName in kind.fileNamesFor(exportId)) {
      val candidate = appDataRoot.resolve("native-file-jobs/handoffs/$fileName")
      resolveManagedHandoffSource(appDataRoot, candidate.absolutePath, kind.nameRegex)
        ?.let { return it }
    }
  }
  val screenshot = appDataRoot.resolve(
    "native-file-jobs/screenshot-output/$exportId/$MANAGED_SCREENSHOT_FILE",
  )
  return resolveManagedScreenshotSource(appDataRoot, screenshot.absolutePath)
}

internal fun discardManagedScreenshotSource(appDataRoot: File, exportId: String): Boolean {
  if (!isCanonicalUuidV4(exportId)) return false
  val source = appDataRoot.resolve(
    "native-file-jobs/screenshot-output/$exportId/$MANAGED_SCREENSHOT_FILE",
  )
  val owned = resolveManagedScreenshotSource(appDataRoot, source.absolutePath) ?: return false
  return runCatching { owned.parentFile?.deleteRecursively() == true }.getOrDefault(false)
}

internal fun prepareManagedExportAcknowledgement(
  appDataRoot: File,
  exportId: String,
  sourceKind: SafDestinationSourceKind,
): Boolean {
  if (sourceKind != SafDestinationSourceKind.SCREENSHOT) return true
  val directory = appDataRoot.resolve("native-file-jobs/screenshot-output/$exportId")
  if (!directory.exists()) return true
  return discardManagedScreenshotSource(appDataRoot, exportId)
}

internal fun requiresRisuSavePublicationProof(
  appDataRoot: File,
  exportId: String,
  sourceKind: SafDestinationSourceKind,
): Boolean {
  if (sourceKind != SafDestinationSourceKind.RISU_SAVE) return false
  val source = resolveManagedExportById(appDataRoot, exportId) ?: return true
  return MANAGED_EXPORT_NAME.matches(source.name)
}

private fun withPartialCleanup(
  error: SafDestinationException,
  deletePartial: () -> Boolean,
  createdDocument: Boolean,
): SafDestinationException {
  if (!createdDocument) {
    return SafDestinationException(
      error.code,
      (error.warningCodes + "partial-destination-may-remain").distinct(),
      error.message ?: "Android SAF destination copy failed",
      error,
    )
  }
  val removed = runCatching(deletePartial).getOrDefault(false)
  return if (removed) error else SafDestinationException(
    error.code,
    (error.warningCodes + "partial-destination-may-remain").distinct(),
    error.message ?: "Android SAF destination copy failed",
    error,
  )
}

private fun checkCancellation(isCancelled: () -> Boolean) {
  if (isCancelled()) {
    throw SafSpoolException("cancelled", "SAF source copy was cancelled")
  }
}

private fun checkDestinationCancellation(
  isCancelled: () -> Boolean,
  warnings: List<String>,
) {
  if (isCancelled()) {
    throw SafDestinationException("cancelled", warnings, "Android SAF destination copy was cancelled")
  }
}

private class SafSpoolException(
  val code: String,
  message: String,
  cause: Throwable? = null,
) : IOException(message, cause)

internal fun safeSafDisplayName(name: String): String {
  val leaf = name.substringAfterLast('/').substringAfterLast('\\')
  val safe = leaf.replace(Regex("[^A-Za-z0-9._-]"), "_")
  if (safe.isBlank()) return "opened-file"
  if (safe.length <= MAX_DISPLAY_NAME_CHARS) return safe
  val suffix = NATIVE_FILE_JOB_SPOOL_SUFFIXES.firstOrNull { extension ->
    safe.endsWith(extension, ignoreCase = true)
  }?.let { extension -> safe.takeLast(extension.length) }
  return if (suffix == null) {
    safe.take(MAX_DISPLAY_NAME_CHARS)
  } else {
    safe.take(MAX_DISPLAY_NAME_CHARS - suffix.length) + suffix
  }
}

internal fun safeSafDestinationName(name: String): String {
  val safe = safeSafDisplayName(name)
  return if (
    isBackupSource(safe)
    || safe.endsWith(".zip", ignoreCase = true)
    || safe.endsWith(".charx", ignoreCase = true)
    || safe.endsWith(".jpeg", ignoreCase = true)
    || safe.endsWith(".json", ignoreCase = true)
    || safe.endsWith(".png", ignoreCase = true)
    || safe.endsWith(".risum", ignoreCase = true)
  ) safe else "$safe.risudat"
}

private fun readSpoolOwnership(file: File): SafSpoolOwnership? {
  if (!file.isFile || file.length() > 4_096) return null
  val json = file.readText(Charsets.UTF_8)
  val format = stringField(json, "format")
  val version = numberField(json, "version")
  // An empty token must stay unowned; the shared helper also matches "".
  val token = stringField(json, "token")?.takeIf(String::isNotEmpty)
  val createdAtMillis = numberField(json, "createdAtMillis")
  if (
    format != SPOOL_OWNERSHIP_FORMAT ||
    version != 1L ||
    token == null ||
    createdAtMillis == null
  ) return null
  return SafSpoolOwnership(token, createdAtMillis)
}

private fun readReadySpool(directory: File, token: String): SafSpoolReady? {
  val manifest = directory.resolve("source.json")
  if (!manifest.isFile || manifest.length() > 4_096) return null
  val json = runCatching { manifest.readText(Charsets.UTF_8) }.getOrNull() ?: return null
  val manifestToken = stringField(json, "token")
  val state = stringField(json, "state")
  val displayName = stringField(json, "displayName")
  val bytes = numberField(json, "bytes")
  // Not nullableLongField: an unparseable number keeps the batch with a null
  // total instead of rejecting the spool.
  val totalText = Regex("\\\"totalBytes\\\":(null|[0-9]+)")
    .find(json)?.groupValues?.get(1)
  val totalBytes = totalText?.takeUnless { it == "null" }?.toLongOrNull()
  val source = directory.resolve("source.risudat")
  if (
    manifestToken != token ||
    state != "ready" ||
    displayName == null ||
    safeSafDisplayName(displayName) != displayName ||
    bytes == null ||
    totalText == null ||
    !source.isFile ||
    source.length() != bytes
  ) return null
  return SafSpoolReady(token, displayName, bytes, totalBytes)
}

private fun spoolReadyJson(ready: List<SafSpoolReady>): String = ready.joinToString(",") { source ->
  "{" +
    "\"token\":${jsonString(source.token)}," +
    "\"displayName\":${jsonString(source.displayName)}," +
    "\"bytes\":${source.bytes}," +
    (source.totalBytes?.let { "\"totalBytes\":$it" } ?: "\"totalBytes\":null") +
    "}"
}

private fun spoolFailuresJson(failures: List<SafSpoolFailure>): String =
  failures.joinToString(",") { failure ->
    "{\"displayName\":${jsonString(failure.displayName)},\"code\":${jsonString(failure.code)}}"
  }

internal fun androidSpoolBatchScript(requestId: String, batch: SafSpoolBatch): String {
  val ready = spoolReadyJson(batch.ready)
  val failures = spoolFailuresJson(batch.failures)
  val value = "{\"requestId\":${jsonString(requestId)},\"ready\":[$ready],\"failures\":[$failures]}"
  val pending = "{" +
    "\"requestId\":${jsonString(requestId)}," +
    "\"ready\":[...(window.tauriOpenedFileSpools?.ready??[]),...[$ready]]," +
    "\"failures\":[...(window.tauriOpenedFileSpools?.failures??[]),...[$failures]]}"
  return "window.tauriOpenedFileSpools=$pending;" +
    "window.dispatchEvent(new CustomEvent('risu-android-spool-ready',{detail:$value}));"
}

private fun sourcePickedScript(eventName: String, requestId: String, batch: SafSpoolBatch): String {
  val detail = "{" +
    "\"requestId\":${jsonString(requestId)}," +
    "\"ready\":[${spoolReadyJson(batch.ready)}]," +
    "\"failures\":[${spoolFailuresJson(batch.failures)}]}"
  return "window.dispatchEvent(new CustomEvent('$eventName',{detail:$detail}));"
}

internal fun androidBackupSourcePickedScript(requestId: String, batch: SafSpoolBatch): String =
  sourcePickedScript("risu-android-backup-source-picked", requestId, batch)

internal fun androidBackupSourceResultScript(
  requestId: String,
  batch: SafSpoolBatch,
  restored: Boolean,
): String = if (restored) {
  androidSpoolBatchScript(requestId, batch)
} else {
  androidBackupSourcePickedScript(requestId, batch)
}

internal fun androidLegacyBackupSourcePickedScript(
  requestId: String,
  batch: SafSpoolBatch,
): String = sourcePickedScript("risu-android-legacy-backup-source-picked", requestId, batch)

internal fun androidLegacyBackupSourceResultScript(
  requestId: String,
  batch: SafSpoolBatch,
  restored: Boolean,
): String = if (restored) {
  androidSpoolBatchScript(requestId, batch)
} else {
  androidLegacyBackupSourcePickedScript(requestId, batch)
}

internal fun androidSafProgressScript(
  requestId: String,
  operation: String,
  copiedBytes: Long,
  totalBytes: Long?,
  token: String?,
): String {
  val detail = "{" +
    "\"requestId\":${jsonString(requestId)}," +
    "\"operation\":${jsonString(operation)}," +
    "\"copiedBytes\":$copiedBytes," +
    "\"totalBytes\":${totalBytes ?: "null"}," +
    "\"token\":${token?.let(::jsonString) ?: "null"}" +
    "}"
  return "window.dispatchEvent(new CustomEvent('risu-android-saf-progress',{detail:$detail}));"
}

internal fun androidSafDestinationScript(
  requestId: String,
  exportId: String = requestId,
  sourceKind: SafDestinationSourceKind = SafDestinationSourceKind.RISU_SAVE,
  state: String,
  bytes: Long? = null,
  code: String? = null,
  message: String? = null,
  warningCodes: List<String>,
  publicationPrerequisitesComplete: Boolean = false,
): String {
  val detail = androidSafDestinationJson(
    requestId,
    exportId,
    sourceKind,
    state,
    bytes,
    code,
    message,
    warningCodes,
    publicationPrerequisitesComplete,
  )
  return "window.tauriAndroidSafDestinationResult=$detail;" +
    "window.dispatchEvent(new CustomEvent('risu-android-saf-destination',{detail:$detail}));"
}

internal fun androidSafDestinationJson(
  requestId: String,
  exportId: String = requestId,
  sourceKind: SafDestinationSourceKind = SafDestinationSourceKind.RISU_SAVE,
  state: String,
  bytes: Long? = null,
  code: String? = null,
  message: String? = null,
  warningCodes: List<String>,
  publicationPrerequisitesComplete: Boolean = false,
): String {
  val warnings = warningCodes.joinToString(",") { jsonString(it) }
  return "{" +
    "\"requestId\":${jsonString(requestId)}," +
    "\"exportId\":${jsonString(exportId)}," +
    "\"sourceKind\":${jsonString(sourceKind.wireName)}," +
    "\"state\":${jsonString(state)}," +
    "\"bytes\":${bytes ?: "null"}," +
    "\"code\":${code?.let(::jsonString) ?: "null"}," +
    "\"message\":${message?.let(::jsonString) ?: "null"}," +
    "\"warningCodes\":[$warnings]," +
    "\"publicationPrerequisitesComplete\":$publicationPrerequisitesComplete" +
    "}"
}

private fun jsonString(value: String): String = buildString {
  append('"')
  for (character in value) {
    when {
      character == '\\' -> append("\\\\")
      character == '"' -> append("\\\"")
      character == '\b' -> append("\\b")
      character == '\u000c' -> append("\\f")
      character == '\n' -> append("\\n")
      character == '\r' -> append("\\r")
      character == '\t' -> append("\\t")
      character < ' ' || character == '\u2028' || character == '\u2029' ->
        append("\\u%04x".format(character.code))
      else -> append(character)
    }
  }
  append('"')
}

internal fun isNativeContentSource(name: String): Boolean =
  listOf(".json", ".png", ".charx", ".jpg", ".jpeg", ".risum", ".lorebook").any { name.endsWith(it, ignoreCase = true) }

internal fun androidContentSourcePickedScript(requestId: String, batch: SafSpoolBatch): String =
  sourcePickedScript("risu-android-content-source-picked", requestId, batch)
