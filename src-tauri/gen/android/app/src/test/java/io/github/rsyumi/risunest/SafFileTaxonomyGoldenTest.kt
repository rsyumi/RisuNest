package io.github.rsyumi.risunest

import java.io.File
import javax.xml.parsers.DocumentBuilderFactory
import org.w3c.dom.Element
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

/**
 * Characterization guard for the native export/import filename taxonomy that
 * is independently hardcoded in Kotlin (SafFileBridge), Rust, and TypeScript.
 * Each side pins itself to the same golden fixture
 * (src/ts/storage/tests/fixtures/nativeFileTaxonomyV1Golden.json); a drift on
 * any side fails that side's suite.
 */
class SafFileTaxonomyGoldenTest {
  @get:Rule val temporaryFolder = TemporaryFolder.builder().assureDeletion().build()

  private data class HandoffGrammar(val kind: String, val prefix: String, val suffixes: List<String>)

  private data class Taxonomy(
    val uuid: String,
    val databaseSuffixes: List<String>,
    val contentSuffixes: List<String>,
    val grammars: List<HandoffGrammar>,
    val exportDirectory: String,
    val exportPrefix: String,
    val exportDataSuffix: String,
    val exportLeaseSuffix: String,
    val rejectedModuleNames: List<String>,
  )

  private fun fixtureFile(): File {
    var current: File? = File(System.getProperty("user.dir")).absoluteFile
    repeat(8) {
      val candidate = current?.resolve("src/ts/storage/tests/fixtures/nativeFileTaxonomyV1Golden.json")
      if (candidate != null && candidate.isFile) return candidate
      current = current?.parentFile
    }
    error("nativeFileTaxonomyV1Golden.json was not found above ${System.getProperty("user.dir")}")
  }

  private fun quotedStrings(fragment: String): List<String> =
    Regex("\"([^\"]*)\"").findAll(fragment).map { it.groupValues[1] }.toList()

  private fun loadTaxonomy(): Taxonomy {
    val json = fixtureFile().readText(Charsets.UTF_8)
    fun array(name: String): String =
      Regex("\"$name\"\\s*:\\s*\\[([^\\]]*)\\]").find(json)?.groupValues?.get(1)
        ?: error("fixture array $name is missing")
    fun string(name: String): String =
      Regex("\"$name\"\\s*:\\s*\"([^\"]*)\"").find(json)?.groupValues?.get(1)
        ?: error("fixture string $name is missing")
    val grammars = Regex(
      "\\{\\s*\"kind\"\\s*:\\s*\"([^\"]+)\"\\s*,\\s*\"prefix\"\\s*:\\s*\"([^\"]+)\"\\s*," +
        "\\s*\"suffixes\"\\s*:\\s*\\[([^\\]]*)\\]\\s*\\}",
    ).findAll(json).map { match ->
      HandoffGrammar(match.groupValues[1], match.groupValues[2], quotedStrings(match.groupValues[3]))
    }.toList()
    check(grammars.size == 6) { "expected 6 managed handoff grammars, found ${grammars.size}" }
    val exportBlock = Regex("\"risuSaveExport\"\\s*:\\s*\\{([^}]*)\\}").find(json)?.groupValues?.get(1)
      ?: error("fixture risuSaveExport block is missing")
    fun exportString(name: String): String =
      Regex("\"$name\"\\s*:\\s*\"([^\"]*)\"").find(exportBlock)?.groupValues?.get(1)
        ?: error("fixture risuSaveExport.$name is missing")
    return Taxonomy(
      uuid = string("uuid"),
      databaseSuffixes = quotedStrings(array("database")),
      contentSuffixes = quotedStrings(array("content")),
      grammars = grammars,
      exportDirectory = exportString("directory"),
      exportPrefix = exportString("prefix"),
      exportDataSuffix = exportString("dataSuffix"),
      exportLeaseSuffix = exportString("leaseSuffix"),
      rejectedModuleNames = quotedStrings(array("rejectedModuleNames")),
    )
  }

  private fun temporaryAppDataRoot(): File = temporaryFolder.newFolder()

  @Test
  fun `spool allowlist equals the fixture database and content union`() {
    val taxonomy = loadTaxonomy()
    val union = taxonomy.databaseSuffixes + taxonomy.contentSuffixes
    assertEquals(union.size, union.toSet().size)
    for (suffix in union) {
      assertTrue(suffix, shouldUseNativeFileJobSpool("import$suffix"))
      assertTrue(suffix, shouldUseNativeFileJobSpool("IMPORT${suffix.uppercase()}"))
    }
    assertFalse(shouldUseNativeFileJobSpool("import.txt"))
    assertFalse(shouldUseNativeFileJobSpool("import"))
    assertFalse(shouldUseNativeFileJobSpool("import.risulossless"))
  }

  @Test
  fun `common backup picker and open with registration cover current database suffixes`() {
    val taxonomy = loadTaxonomy()
    assertEquals(listOf(".risunest", ".risudat", ".bin"), taxonomy.databaseSuffixes)
    for (suffix in taxonomy.databaseSuffixes) {
      assertTrue(suffix, isBackupSource("backup$suffix"))
      assertTrue(suffix, isBackupSource("BACKUP${suffix.uppercase()}"))
    }
    assertFalse(isBackupSource("backup.risulossless"))
    assertFalse(isBackupSource("backup.risunest.txt"))
    for (suffix in taxonomy.contentSuffixes) {
      assertFalse(suffix, isBackupSource("content$suffix"))
    }

    var repository = fixtureFile()
    repeat(6) { repository = requireNotNull(repository.parentFile) }
    val manifest = repository.resolve("src-tauri/gen/android/app/src/main/AndroidManifest.xml")
    val factory = DocumentBuilderFactory.newInstance().apply { isNamespaceAware = true }
    val document = factory.newDocumentBuilder().parse(manifest)
    val filters = document.getElementsByTagName("intent-filter")
    val androidNamespace = "http://schemas.android.com/apk/res/android"
    val registered = mutableMapOf<String, MutableSet<String>>()
    var mimeAssociationFound = false
    for (index in 0 until filters.length) {
      val filter = filters.item(index) as Element
      val actions = filter.getElementsByTagName("action")
      val actionNames = (0 until actions.length).map {
        (actions.item(it) as Element).getAttributeNS(androidNamespace, "name")
      }.toSet()
      if (!actionNames.contains("android.intent.action.VIEW")) continue
      val data = filter.getElementsByTagName("data")
      val dataElements = (0 until data.length).map { data.item(it) as Element }
      if (dataElements.any {
          it.getAttributeNS(androidNamespace, "mimeType") == "application/x-risunest"
        }) {
        assertTrue(actionNames.contains("android.intent.action.SEND"))
        assertTrue(actionNames.contains("android.intent.action.SEND_MULTIPLE"))
        mimeAssociationFound = true
      }
      val schemes = dataElements.map {
        it.getAttributeNS(androidNamespace, "scheme")
      }.filter { it == "content" || it == "file" }.toSet()
      if (schemes.isEmpty()) continue
      assertEquals(setOf("*"), dataElements.map {
        it.getAttributeNS(androidNamespace, "host")
      }.filter { it.isNotEmpty() }.toSet())
      val patterns = mutableSetOf<String>()
      for (dataIndex in 0 until data.length) {
        patterns.add((data.item(dataIndex) as Element).getAttributeNS(androidNamespace, "pathPattern"))
      }
      for (scheme in schemes) {
        registered.getOrPut(scheme) { mutableSetOf() }.addAll(patterns)
      }
    }
    assertTrue(mimeAssociationFound)
    assertEquals(setOf("content", "file"), registered.keys)
    val configuration = repository.resolve("src-tauri/tauri.conf.json").readText(Charsets.UTF_8)
    val configured = Regex("\"ext\"\\s*:\\s*\\[([^\\]]*)\\]")
      .findAll(configuration).flatMap { quotedStrings(it.groupValues[1]) }.toSet()
    for (suffix in taxonomy.databaseSuffixes) {
      if (suffix == ".bin") {
        // Generic binary backups remain explicit picker inputs, not an OS-wide association.
        assertTrue(registered.values.none { it.contains(".*\\\\$suffix") })
        assertFalse(configured.contains("bin"))
      } else {
        for (scheme in listOf("content", "file")) {
          assertTrue("$scheme:$suffix", registered.getValue(scheme).contains(".*\\\\$suffix"))
        }
        assertTrue(suffix, configured.contains(suffix.removePrefix(".")))
      }
    }
    assertTrue(registered.values.none { it.contains(".*\\\\.risulossless") })
    assertFalse(configured.contains("risulossless"))
  }

  @Test
  fun `fixture uuid is canonical and its uppercase twin is rejected`() {
    val taxonomy = loadTaxonomy()
    assertTrue(isCanonicalUuidV4(taxonomy.uuid))
    assertFalse(isCanonicalUuidV4(taxonomy.uuid.uppercase()))
  }

  @Test
  fun `managed handoff grammars resolve and malformed module names never resolve`() {
    val taxonomy = loadTaxonomy()
    val appDataRoot = temporaryAppDataRoot()
    val handoffs = appDataRoot.resolve("native-file-jobs/handoffs")
    assertTrue(handoffs.mkdirs())
    for (grammar in taxonomy.grammars) {
      val expectedSourceKind = when (grammar.kind) {
        "legacy-backup" -> SafDestinationSourceKind.LEGACY_BACKUP
        "raw-recovery", "portable-backup", "character-charx", "character-card", "risu-module" ->
          SafDestinationSourceKind.RISU_SAVE
        else -> error("unexpected managed handoff kind ${grammar.kind}")
      }
      for (suffix in grammar.suffixes) {
        val source = handoffs.resolve("${grammar.prefix}${taxonomy.uuid}$suffix")
        source.writeBytes(byteArrayOf(1))
        assertNotNull(
          "${source.name} must resolve as a managed ${grammar.kind} handoff",
          resolveManagedExportSource(appDataRoot, source.absolutePath),
        )
        assertEquals(source.name, taxonomy.uuid, managedExportId(source))
        assertEquals(source.name, expectedSourceKind, managedExportSourceKind(source))
        assertTrue(source.delete())
      }
    }
    for (rejected in taxonomy.rejectedModuleNames) {
      val source = handoffs.resolve(rejected)
      source.writeBytes(byteArrayOf(1))
      assertNull(
        "$rejected must not resolve as a managed handoff",
        resolveManagedExportSource(appDataRoot, source.absolutePath),
      )
      assertTrue(source.delete())
    }
  }

  @Test
  fun `risusave export resolves only with a matching lease`() {
    val taxonomy = loadTaxonomy()
    val appDataRoot = temporaryAppDataRoot()
    val exports = appDataRoot.resolve(taxonomy.exportDirectory)
    assertTrue(exports.mkdirs())
    val name = "${taxonomy.exportPrefix}${taxonomy.uuid}${taxonomy.exportDataSuffix}"
    val source = exports.resolve(name)
    source.writeBytes(byteArrayOf(1))

    assertNull(resolveManagedExportSource(appDataRoot, source.absolutePath))

    val lease = exports.resolve("${taxonomy.exportPrefix}${taxonomy.uuid}${taxonomy.exportLeaseSuffix}")
    lease.writeText("{\"exportId\":\"not-the-same-id\"}", Charsets.UTF_8)
    assertNull(resolveManagedExportSource(appDataRoot, source.absolutePath))

    lease.writeText("{\"exportId\":\"${taxonomy.uuid}\"}", Charsets.UTF_8)
    assertNotNull(resolveManagedExportSource(appDataRoot, source.absolutePath))
  }
}
