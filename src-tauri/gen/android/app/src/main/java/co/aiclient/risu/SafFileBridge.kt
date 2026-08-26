package co.aiclient.risu

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
private val CANONICAL_TOKEN = Regex(
  "[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
)

internal fun isCanonicalUuidV4(value: String): Boolean = CANONICAL_TOKEN.matches(value)
private val MANAGED_EXPORT_NAME = Regex(
  "risusave-([0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})\\.risudat",
)

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
    source.inputStream().use { input ->
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

internal fun resolveManagedExportSource(appDataRoot: File, sourcePath: String): File? {
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

internal fun managedExportId(source: File): String? =
  MANAGED_EXPORT_NAME.matchEntire(source.name)?.groupValues?.get(1)

internal fun resolveManagedExportById(appDataRoot: File, exportId: String): File? {
  if (!isCanonicalUuidV4(exportId)) return null
  val source = appDataRoot.resolve("persistent/exports/risusave-$exportId.risudat")
  return resolveManagedExportSource(appDataRoot, source.absolutePath)
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
  val safe = leaf.replace(Regex("[^A-Za-z0-9._-]"), "_").take(MAX_DISPLAY_NAME_CHARS)
  return safe.ifBlank { "opened-file" }
}

internal fun safeSafDestinationName(name: String): String {
  val safe = safeSafDisplayName(name)
  return if (safe.endsWith(".risudat", ignoreCase = true)) safe else "$safe.risudat"
}

private fun readSpoolOwnership(file: File): SafSpoolOwnership? {
  if (!file.isFile || file.length() > 4_096) return null
  val json = file.readText(Charsets.UTF_8)
  val format = Regex("\\\"format\\\":\\\"([^\\\"]+)\\\"")
    .find(json)?.groupValues?.get(1)
  val version = Regex("\\\"version\\\":([0-9]+)")
    .find(json)?.groupValues?.get(1)?.toIntOrNull()
  val token = Regex("\\\"token\\\":\\\"([^\\\"]+)\\\"")
    .find(json)?.groupValues?.get(1)
  val createdAtMillis = Regex("\\\"createdAtMillis\\\":([0-9]+)")
    .find(json)?.groupValues?.get(1)?.toLongOrNull()
  if (
    format != SPOOL_OWNERSHIP_FORMAT ||
    version != 1 ||
    token == null ||
    createdAtMillis == null
  ) return null
  return SafSpoolOwnership(token, createdAtMillis)
}

private fun readReadySpool(directory: File, token: String): SafSpoolReady? {
  val manifest = directory.resolve("source.json")
  if (!manifest.isFile || manifest.length() > 4_096) return null
  val json = runCatching { manifest.readText(Charsets.UTF_8) }.getOrNull() ?: return null
  val manifestToken = Regex("\\\"token\\\":\\\"([^\\\"]+)\\\"")
    .find(json)?.groupValues?.get(1)
  val state = Regex("\\\"state\\\":\\\"([^\\\"]+)\\\"")
    .find(json)?.groupValues?.get(1)
  val displayName = Regex("\\\"displayName\\\":\\\"([^\\\"]+)\\\"")
    .find(json)?.groupValues?.get(1)
  val bytes = Regex("\\\"bytes\\\":([0-9]+)")
    .find(json)?.groupValues?.get(1)?.toLongOrNull()
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

internal fun androidSpoolBatchScript(requestId: String, batch: SafSpoolBatch): String {
  val ready = batch.ready.joinToString(",") { source ->
    "{" +
      "\"token\":${jsonString(source.token)}," +
      "\"displayName\":${jsonString(source.displayName)}," +
      "\"bytes\":${source.bytes}," +
      (source.totalBytes?.let { "\"totalBytes\":$it" } ?: "\"totalBytes\":null") +
      "}"
  }
  val failures = batch.failures.joinToString(",") { failure ->
    "{\"displayName\":${jsonString(failure.displayName)},\"code\":${jsonString(failure.code)}}"
  }
  val value = "{\"requestId\":${jsonString(requestId)},\"ready\":[$ready],\"failures\":[$failures]}"
  return "window.tauriOpenedFileSpools=$value;" +
    "window.dispatchEvent(new CustomEvent('risu-android-spool-ready',{detail:$value}));"
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
  state: String,
  bytes: Long? = null,
  code: String? = null,
  message: String? = null,
  warningCodes: List<String>,
): String {
  val detail = androidSafDestinationJson(
    requestId,
    state,
    bytes,
    code,
    message,
    warningCodes,
  )
  return "window.tauriAndroidSafDestinationResult=$detail;" +
    "window.dispatchEvent(new CustomEvent('risu-android-saf-destination',{detail:$detail}));"
}

internal fun androidSafDestinationJson(
  requestId: String,
  state: String,
  bytes: Long? = null,
  code: String? = null,
  message: String? = null,
  warningCodes: List<String>,
): String {
  val warnings = warningCodes.joinToString(",") { jsonString(it) }
  return "{" +
    "\"requestId\":${jsonString(requestId)}," +
    "\"state\":${jsonString(state)}," +
    "\"bytes\":${bytes ?: "null"}," +
    "\"code\":${code?.let(::jsonString) ?: "null"}," +
    "\"message\":${message?.let(::jsonString) ?: "null"}," +
    "\"warningCodes\":[$warnings]" +
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
