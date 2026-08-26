package co.aiclient.risu

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
private val CANONICAL_TOKEN = Regex(
  "[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
)
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

internal class SafSpoolStore(
  root: File,
  private val bufferBytes: Int = DEFAULT_COPY_BUFFER_BYTES,
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
      val ownedDirectory = root.resolve(token)
      try {
        if (!ownedDirectory.mkdir()) {
          throw SafSpoolException("spool-create-failed", "SAF spool directory cannot be created")
        }
        writeManifest(
          ownedDirectory,
          token,
          "copying",
          displayName,
          bytes = null,
          totalBytes = source.totalBytes,
        )
        val copiedBytes = copySource(
          source,
          ownedDirectory.resolve("source.risudat"),
          token,
          isCancelled,
          onProgress,
        )
        writeManifest(
          ownedDirectory,
          token,
          "ready",
          displayName,
          bytes = copiedBytes,
          totalBytes = source.totalBytes,
        )
        ready.add(SafSpoolReady(token, displayName, copiedBytes, source.totalBytes))
      } catch (error: SafSpoolException) {
        deleteOwnedDirectory(ownedDirectory, token)
        failures.add(SafSpoolFailure(displayName, error.code))
      } catch (error: Exception) {
        deleteOwnedDirectory(ownedDirectory, token)
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
      val token = candidate.name
      if (!candidate.isDirectory || !CANONICAL_TOKEN.matches(token) || token in activeTokens) continue
      val canonicalCandidate = runCatching { candidate.canonicalFile }.getOrNull() ?: continue
      if (canonicalCandidate.parentFile != canonicalRoot) continue
      val manifest = candidate.resolve("source.json")
      val manifestToken = runCatching { readManifestToken(manifest) }.getOrNull() ?: continue
      if (manifestToken != token) continue
      val modifiedAt = maxOf(candidate.lastModified(), manifest.lastModified())
      if (nowMillis - modifiedAt < staleAfterMillis) continue
      if (deleteOwnedDirectory(candidate, token)) removed.add(token)
    }
    return removed.sorted()
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
    val manifest = directory.resolve("source.json")
    val json = "{" +
      "\"token\":${jsonString(token)}," +
      "\"state\":${jsonString(state)}," +
      "\"display_name\":${jsonString(displayName)}," +
      "\"bytes\":${bytes ?: "null"}," +
      "\"total_bytes\":${totalBytes ?: "null"}" +
      "}"
    try {
      FileOutputStream(manifest, false).use { output ->
        output.write(json.toByteArray(Charsets.UTF_8))
        output.flush()
        output.fd.sync()
      }
    } catch (error: Exception) {
      throw SafSpoolException("spool-write-failed", "SAF spool manifest cannot be written", error)
    }
  }

  private fun deleteOwnedDirectory(directory: File, token: String): Boolean {
    if (!CANONICAL_TOKEN.matches(token)) return false
    val canonicalRoot = runCatching { root.canonicalFile }.getOrNull() ?: return false
    val canonicalDirectory = runCatching { directory.canonicalFile }.getOrNull() ?: return false
    if (canonicalDirectory.parentFile != canonicalRoot || canonicalDirectory.name != token) return false
    var deleted = true
    for (name in listOf("source.risudat", "source.json")) {
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

private fun readManifestToken(manifest: File): String? {
  if (!manifest.isFile || manifest.length() > 4_096) return null
  return Regex("\\\"token\\\":\\\"([^\\\"]+)\\\"")
    .find(manifest.readText(Charsets.UTF_8))
    ?.groupValues
    ?.get(1)
}

internal fun androidSpoolBatchScript(batch: SafSpoolBatch): String {
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
  val value = "{\"ready\":[$ready],\"failures\":[$failures]}"
  return "window.tauriOpenedFileSpools=$value;" +
    "window.dispatchEvent(new CustomEvent('risu-android-spool-ready',{detail:$value}));"
}

internal fun androidSafDestinationScript(
  requestId: String,
  state: String,
  bytes: Long? = null,
  code: String? = null,
  message: String? = null,
  warningCodes: List<String>,
): String {
  val warnings = warningCodes.joinToString(",") { jsonString(it) }
  val detail = "{" +
    "\"requestId\":${jsonString(requestId)}," +
    "\"state\":${jsonString(state)}," +
    "\"bytes\":${bytes ?: "null"}," +
    "\"code\":${code?.let(::jsonString) ?: "null"}," +
    "\"message\":${message?.let(::jsonString) ?: "null"}," +
    "\"warningCodes\":[$warnings]" +
    "}"
  return "window.dispatchEvent(new CustomEvent('risu-android-saf-destination',{detail:$detail}));"
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
