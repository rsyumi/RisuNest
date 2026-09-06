package io.github.rsyumi.risunest

import java.io.File
import java.io.InputStream
import java.io.OutputStream
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SafFileBridgeLowMemoryTest {
  @Test
  fun `copies a virtual five GiB source within the fixed buffer`() = runBlocking {
    val maxHeap = Runtime.getRuntime().maxMemory()
    assertTrue("low-memory test heap exceeded 40 MiB: $maxHeap", maxHeap <= 40L * MIB)
    val input = VirtualInputStream(5L * GIB)
    val output = CountingOutputStream()

    val result = copySafDestinationOnIo(
      source = File("virtual-source.risudat"),
      openSource = { input },
      openDestination = { output },
      deletePartial = { true },
      createdDocument = true,
    )

    assertEquals(5L * GIB, result.bytes)
    assertEquals(5L * GIB, output.bytesWritten)
    assertTrue(output.largestWrite <= 64 * 1024)
    assertEquals(0L, input.remaining)
  }

  private class VirtualInputStream(var remaining: Long) : InputStream() {
    override fun read(): Int {
      if (remaining == 0L) return -1
      remaining -= 1
      return 0
    }

    override fun read(bytes: ByteArray, offset: Int, length: Int): Int {
      if (remaining == 0L) return -1
      val count = minOf(length.toLong(), remaining).toInt()
      remaining -= count
      return count
    }
  }

  private class CountingOutputStream : OutputStream() {
    var bytesWritten = 0L
    var largestWrite = 0

    override fun write(value: Int) {
      bytesWritten += 1
      largestWrite = maxOf(largestWrite, 1)
    }

    override fun write(bytes: ByteArray, offset: Int, length: Int) {
      bytesWritten += length
      largestWrite = maxOf(largestWrite, length)
    }
  }

  private companion object {
    const val MIB = 1024L * 1024L
    const val GIB = 1024L * MIB
  }
}
