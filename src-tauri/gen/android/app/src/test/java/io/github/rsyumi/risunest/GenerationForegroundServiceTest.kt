package io.github.rsyumi.risunest

import android.app.Service
import java.io.File
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element

class GenerationForegroundServiceTest {
  @Test
  fun `only first begin starts and only final end stops`() {
    val lifecycle = GenerationForegroundLifecycle()
    var starts = 0
    var stops = 0

    assertTrue(lifecycle.begin { starts += 1; true })
    assertTrue(lifecycle.begin { starts += 1; true })
    assertTrue(lifecycle.end { stops += 1; true })
    assertTrue(lifecycle.end { stops += 1; true })
    assertFalse(lifecycle.end { stops += 1; true })

    assertEquals(1, starts)
    assertEquals(1, stops)
  }

  @Test
  fun `teardown clears every nested generation and stops once`() {
    val lifecycle = GenerationForegroundLifecycle()
    var stops = 0

    assertTrue(lifecycle.begin { true })
    assertTrue(lifecycle.begin { true })
    assertTrue(lifecycle.stopAll { stops += 1; true })
    assertFalse(lifecycle.end { stops += 1; true })

    assertEquals(1, stops)
  }

  @Test
  fun `repeated teardown is idempotent and recreation begins from zero`() {
    val lifecycle = GenerationForegroundLifecycle()
    var starts = 0
    var stops = 0

    assertTrue(lifecycle.begin { starts += 1; true })
    assertTrue(lifecycle.stopAll { stops += 1; true })
    assertFalse(lifecycle.stopAll { stops += 1; true })
    assertTrue(lifecycle.begin { starts += 1; true })
    assertTrue(lifecycle.end { stops += 1; true })

    assertEquals(2, starts)
    assertEquals(2, stops)
  }

  @Test
  fun `queued start invalidated by teardown cannot reactivate after recreation`() {
    val lifecycle = GenerationForegroundLifecycle()
    var staleToken = -1L
    var freshToken = -1L
    var foregroundStarts = 0
    var staleStops = 0

    assertTrue(lifecycle.begin { token -> staleToken = token; true })
    assertTrue(lifecycle.stopAll { true })
    assertTrue(lifecycle.begin { token -> freshToken = token; true })

    assertFalse(lifecycle.activate(staleToken, 100, { foregroundStarts += 1 }, { staleStops += 1 }))
    assertTrue(lifecycle.activate(freshToken, 101, { foregroundStarts += 1 }, { staleStops += 1 }))

    assertEquals(1, foregroundStarts)
    assertEquals(1, staleStops)
  }

  @Test
  fun `failed first start leaves no acquired generation`() {
    val lifecycle = GenerationForegroundLifecycle()
    var stops = 0

    assertFalse(lifecycle.begin { false })
    assertFalse(lifecycle.end { stops += 1; true })

    assertEquals(0, stops)
  }

  @Test
  fun `failed final stop clears the generation without an end retry`() {
    val lifecycle = GenerationForegroundLifecycle()
    var starts = 0
    var stops = 0
    var firstToken = -1L

    assertTrue(lifecycle.begin { token -> firstToken = token; starts += 1; true })
    assertTrue(lifecycle.activate(firstToken, 10, {}, {}))
    assertFalse(lifecycle.end { stops += 1; false })
    assertFalse(lifecycle.end { stops += 1; true })
    assertTrue(lifecycle.begin { starts += 1; true })

    assertEquals(2, starts)
    assertEquals(1, stops)
  }

  @Test
  fun `timeout clears nested generations and late ends stay stopped`() {
    val lifecycle = GenerationForegroundLifecycle()
    var stops = 0
    var staleStops = 0
    var token = -1L

    assertTrue(lifecycle.begin { activeToken -> token = activeToken; true })
    assertTrue(lifecycle.activate(token, 20, {}, {}))
    assertTrue(lifecycle.begin { true })
    assertTrue(lifecycle.timeout(token, 20, { stops += 1 }, { staleStops += 1 }))
    assertFalse(lifecycle.end { stops += 1; true })
    assertFalse(lifecycle.end { stops += 1; true })

    assertEquals(1, stops)
    assertEquals(0, staleStops)
  }

  @Test
  fun `old timeout waits for an in flight fresh begin and stays stale`() {
    val lifecycle = GenerationForegroundLifecycle()
    var oldToken = -1L
    assertTrue(lifecycle.begin { token -> oldToken = token; true })
    assertTrue(lifecycle.activate(oldToken, 30, {}, {}))
    assertTrue(lifecycle.end { true })
    val events = Collections.synchronizedList(mutableListOf<String>())
    val dispatchEntered = CountDownLatch(1)
    val releaseDispatch = CountDownLatch(1)
    val timeoutWasCurrent = AtomicBoolean(true)

    val beginThread = thread(isDaemon = true) {
      lifecycle.begin {
        events.add("start-enter")
        dispatchEntered.countDown()
        assertTrue(releaseDispatch.await(5, TimeUnit.SECONDS))
        events.add("start-exit")
        true
      }
    }
    assertTrue(dispatchEntered.await(5, TimeUnit.SECONDS))
    val timeoutThread = thread(isDaemon = true) {
      timeoutWasCurrent.set(lifecycle.timeout(
        token = oldToken,
        startId = 30,
        stopTimedOut = { events.add("current-stop") },
        stopStaleTimeout = { events.add("stale-stop") },
      ))
    }

    releaseDispatch.countDown()
    beginThread.join(5_000)
    timeoutThread.join(5_000)

    assertFalse(beginThread.isAlive)
    assertFalse(timeoutThread.isAlive)
    assertFalse(timeoutWasCurrent.get())
    assertEquals(listOf("start-enter", "start-exit", "stale-stop"), events)
    assertTrue(lifecycle.end { events.add("final-stop"); true })
    assertEquals(listOf("start-enter", "start-exit", "stale-stop", "final-stop"), events)
  }

  @Test
  fun `begin waits for timeout physical stop before starting again`() {
    val lifecycle = GenerationForegroundLifecycle()
    var token = -1L
    assertTrue(lifecycle.begin { activeToken -> token = activeToken; true })
    assertTrue(lifecycle.activate(token, 40, {}, {}))
    val events = Collections.synchronizedList(mutableListOf<String>())
    val stopEntered = CountDownLatch(1)
    val releaseStop = CountDownLatch(1)

    val timeoutThread = thread(isDaemon = true) {
      lifecycle.timeout(
        token = token,
        startId = 40,
        stopTimedOut = {
          events.add("stop-enter")
          stopEntered.countDown()
          assertTrue(releaseStop.await(5, TimeUnit.SECONDS))
          events.add("stop-exit")
        },
        stopStaleTimeout = { events.add("stale-stop") },
      )
    }
    assertTrue(stopEntered.await(5, TimeUnit.SECONDS))
    val beginThread = thread(isDaemon = true) {
      lifecycle.begin { events.add("start"); true }
    }

    releaseStop.countDown()
    timeoutThread.join(5_000)
    beginThread.join(5_000)

    assertFalse(timeoutThread.isAlive)
    assertFalse(beginThread.isAlive)
    assertEquals(listOf("stop-enter", "stop-exit", "start"), events)
  }

  @Test
  fun `start intent invalidated by timeout cannot reactivate the service`() {
    val lifecycle = GenerationForegroundLifecycle()
    var firstToken = -1L
    var secondToken = -1L
    var foregroundStarts = 0
    var staleStops = 0

    assertTrue(lifecycle.begin { token -> firstToken = token; true })
    assertTrue(lifecycle.activate(firstToken, 50, {}, {}))
    assertTrue(lifecycle.timeout(firstToken, 50, {}, {}))
    assertTrue(lifecycle.begin { token -> secondToken = token; true })

    assertFalse(lifecycle.activate(firstToken, 50, { foregroundStarts += 1 }, { staleStops += 1 }))
    assertTrue(lifecycle.activate(secondToken, 51, { foregroundStarts += 1 }, { staleStops += 1 }))
    assertEquals(1, foregroundStarts)
    assertEquals(1, staleStops)
  }

  @Test
  fun `old queued timeout cannot clear a freshly activated generation`() {
    val lifecycle = GenerationForegroundLifecycle()
    var oldToken = -1L
    var freshToken = -1L
    var currentStops = 0
    var staleStops = 0
    var finalStops = 0

    assertTrue(lifecycle.begin { token -> oldToken = token; true })
    assertTrue(lifecycle.activate(oldToken, 60, {}, {}))
    assertTrue(lifecycle.end { true })
    assertTrue(lifecycle.begin { token -> freshToken = token; true })
    assertTrue(lifecycle.activate(freshToken, 60, {}, {}))

    assertFalse(lifecycle.timeout(oldToken, 60, { currentStops += 1 }, { staleStops += 1 }))
    assertTrue(lifecycle.end { finalStops += 1; true })

    assertEquals(0, currentStops)
    assertEquals(1, staleStops)
    assertEquals(1, finalStops)
  }

  @Test
  fun `stale service destruction cannot clear a fresh generation`() {
    val lifecycle = GenerationForegroundLifecycle()
    var oldToken = -1L
    var freshToken = -1L
    var finalStops = 0

    assertTrue(lifecycle.begin { token -> oldToken = token; true })
    assertTrue(lifecycle.activate(oldToken, 80, {}, {}))
    assertTrue(lifecycle.end { true })
    assertTrue(lifecycle.begin { token -> freshToken = token; true })
    assertTrue(lifecycle.activate(freshToken, 80, {}, {}))

    assertFalse(lifecycle.serviceDestroyed(oldToken, 80))
    assertTrue(lifecycle.end { finalStops += 1; true })

    assertEquals(1, finalStops)
  }

  @Test
  fun `current service destruction clears the generation gate`() {
    val lifecycle = GenerationForegroundLifecycle()
    var token = -1L
    var starts = 0
    var lateStops = 0

    assertTrue(lifecycle.begin { activeToken -> token = activeToken; starts += 1; true })
    assertTrue(lifecycle.activate(token, 90, {}, {}))

    assertTrue(lifecycle.serviceDestroyed(token, 90))
    assertFalse(lifecycle.end { lateStops += 1; true })
    assertTrue(lifecycle.begin { starts += 1; true })

    assertEquals(2, starts)
    assertEquals(0, lateStops)
  }

  @Test
  fun `removing the app task stops the generation service`() {
    val manifest = File("src/main/AndroidManifest.xml")
    val document = javax.xml.parsers.DocumentBuilderFactory.newInstance().apply {
      isNamespaceAware = true
    }.newDocumentBuilder().parse(manifest)
    val services = document.getElementsByTagName("service")
    val generationService = (0 until services.length)
      .map { services.item(it) as Element }
      .single {
        it.getAttributeNS("http://schemas.android.com/apk/res/android", "name") ==
          ".GenerationForegroundService"
      }

    assertEquals(
      "true",
      generationService.getAttributeNS("http://schemas.android.com/apk/res/android", "stopWithTask"),
    )
  }

  @Test
  fun `service is not sticky`() {
    assertEquals(Service.START_NOT_STICKY, GENERATION_FOREGROUND_START_MODE)
  }
}
