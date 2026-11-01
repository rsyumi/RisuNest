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
  private fun taskId(index: Int) = "00000000-0000-4000-8000-%012d".format(index)

  private class Hosted(onHeldChanged: (Boolean) -> Unit = {}) {
    val heldChanges = mutableListOf<Boolean>()
    val registry = BackgroundTaskRegistry { heldChanges.add(it); onHeldChanged(it) }
    val lifecycle = GenerationForegroundLifecycle()
    var starts = 0
    var stops = 0
    var refuse: String? = null
    val expired = mutableListOf<String>()
    val start: () -> String? = {
      if (lifecycle.begin { if (refuse == null) { starts++; true } else false }) null else refuse
    }
    val stop: () -> Boolean = { lifecycle.end { stops++; true } }
    fun keepService(): (() -> Unit)? {
      val token = lifecycle.join() ?: return null
      return { lifecycle.leave(token) { stops++; true } }
    }
    fun begin(id: String, kind: String, deferred: Boolean = false, userInitiated: Boolean = false, refresh: () -> Unit = {}) =
      registry.begin(id, kind, deferred, userInitiated, start, { expired.add(it) }, refresh)
    fun end(id: String) = registry.end(id, stop)
  }

  @Test
  fun `notification refresh failure preserves an admitted owner until end`() {
    val host = Hosted()
    assertEquals(BACKGROUND_ACTIVE, host.begin(taskId(1), "sync"))
    assertEquals(BACKGROUND_ACTIVE, host.begin(taskId(2), "backup") { error("refresh") })
    assertEquals(2, host.registry.snapshot().size)
    assertTrue(host.end(taskId(1)))
    assertEquals(0, host.stops)
    assertTrue(host.end(taskId(2)))
    assertEquals(1, host.stops)
    assertTrue(host.registry.snapshot().isEmpty())
  }

  @Test
  fun `background owners keep independent progress and cannot end a newer task`() {
    val host = Hosted()
    val backup = taskId(1)
    val sync = taskId(2)
    host.begin(backup, "backup")
    host.begin(sync, "sync")
    assertEquals(1, host.starts)
    assertTrue(host.registry.progress(backup, 42))
    assertFalse(host.registry.progress(sync, 101))
    assertEquals(listOf(BackgroundTaskStatus("backup", 42), BackgroundTaskStatus("sync")), host.registry.snapshot())
    assertTrue(host.end(backup))
    assertEquals(0, host.stops)
    assertFalse(host.end(backup))
    assertFalse(host.registry.progress(backup, 99))
    assertTrue(host.end(sync))
    assertEquals(1, host.stops)
    val restore = taskId(3)
    host.begin(restore, "restore")
    host.registry.clear(notify = true)
    assertEquals(listOf(restore), host.expired)
    assertFalse(host.end(restore))
    assertTrue(host.registry.snapshot().isEmpty())
  }

  @Test
  fun `invalid holds own nothing and a refused start stays registered without the service`() {
    val host = Hosted()
    assertEquals(BACKGROUND_INVALID, host.begin(taskId(1), "unknown"))
    assertEquals(BACKGROUND_INVALID, host.begin("not-a-uuid", "backup"))
    assertTrue(host.heldChanges.isEmpty())
    assertEquals(BACKGROUND_ACTIVE, host.begin(taskId(2), "backup"))
    assertEquals(BACKGROUND_INVALID, host.begin(taskId(2), "sync"))
    assertTrue(host.end(taskId(2)))
    assertEquals(1, host.starts)
    assertEquals(1, host.stops)

    host.refuse = BACKGROUND_REFUSED
    assertEquals(BACKGROUND_REFUSED, host.begin(taskId(3), "export"))
    assertTrue(host.registry.snapshot().isEmpty())
    host.refuse = null
    host.registry.promote(host.start)
    assertEquals(listOf(BackgroundTaskStatus("export")), host.registry.snapshot())
    assertEquals(2, host.starts)
  }

  @Test
  fun `deferred holds take the service only while the app is off screen`() {
    val host = Hosted()
    var refreshes = 0
    assertEquals(BACKGROUND_DEFERRED, host.begin(taskId(1), "sync", deferred = true))
    assertEquals(BACKGROUND_DEFERRED, host.begin(taskId(2), "generation", deferred = true))
    assertEquals(0, host.starts)
    assertTrue(host.registry.snapshot().isEmpty())
    // Waiting holds count as held work before the app leaves the screen.
    assertEquals(listOf(true), host.heldChanges)
    assertTrue(host.end(taskId(2)))
    assertEquals(0, host.stops)

    host.registry.promote(host.start) { refreshes++ }
    assertEquals(1, host.starts)
    assertEquals(1, refreshes)
    assertEquals(listOf(BackgroundTaskStatus("sync")), host.registry.snapshot())
    assertEquals(BACKGROUND_ACTIVE, host.begin(taskId(3), "sync", deferred = true))
    assertEquals(1, host.starts)

    host.registry.demote(host.stop)
    assertEquals(1, host.stops)
    assertTrue(host.registry.snapshot().isEmpty())
    assertTrue(host.end(taskId(1)))
    assertEquals(listOf(true), host.heldChanges)
    assertTrue(host.end(taskId(3)))
    assertEquals(1, host.stops)
    assertEquals(listOf(true, false), host.heldChanges)
  }

  @Test
  fun `returning to the screen keeps immediate holds in the service`() {
    val host = Hosted()
    host.begin(taskId(1), "export", userInitiated = true)
    host.begin(taskId(2), "sync", deferred = true)
    host.registry.promote(host.start)
    host.registry.demote(host.stop)
    assertEquals(0, host.stops)
    assertEquals(listOf(BackgroundTaskStatus("export", userInitiated = true)), host.registry.snapshot())
    assertTrue(host.end(taskId(1)))
    assertEquals(1, host.stops)
  }

  @Test
  fun `a refused promotion stops trying and the next leave starts the service again`() {
    val host = Hosted()
    var attempts = 0
    val start: () -> String? = { attempts++; host.start() }
    host.begin(taskId(1), "sync", deferred = true)
    host.begin(taskId(2), "sync", deferred = true)
    host.refuse = BACKGROUND_REFUSED
    host.registry.promote(start)
    assertEquals(1, attempts)
    assertTrue(host.registry.snapshot().isEmpty())

    host.refuse = null
    host.registry.demote(host.stop)
    host.registry.promote(start)
    assertEquals(1, host.starts)
    assertEquals(2, host.registry.snapshot().size)
  }

  @Test
  fun `expiry and destruction end only holds the service protected`() {
    val host = Hosted()
    host.begin(taskId(1), "sync", deferred = true)
    host.begin(taskId(2), "backup")
    host.registry.clear(notify = true)
    assertEquals(listOf(taskId(2)), host.expired)
    host.registry.promote(host.start)
    assertEquals(listOf(BackgroundTaskStatus("sync")), host.registry.snapshot())
    assertEquals(listOf(true), host.heldChanges)
    host.registry.clear()
    assertEquals(listOf(true, false), host.heldChanges)
    host.begin(taskId(3), "backup")
    host.registry.clearAll()
    assertEquals(listOf(true, false, true, false), host.heldChanges)
  }

  private class Delays {
    private val pending = mutableListOf<Runnable>()
    val post: (Runnable, Long) -> Unit = { task, _ -> pending.add(task) }
    val remove: (Runnable) -> Unit = { pending.remove(it) }
    fun elapse() = pending.toList().also { pending.clear() }.forEach(Runnable::run)
  }

  @Test
  fun `the page stays active off screen only for work held when the app left and a moment after it ends`() {
    val delays = Delays()
    val kept = mutableListOf<Boolean>()
    val policy = PageKeptActivePolicy({ kept.add(it) }, { null }, delays.post, delays.remove)
    policy.onResumedChanged(true)
    policy.onHeldChanged(true)
    assertEquals(listOf(true), kept)
    policy.onResumedChanged(false)
    policy.onHeldChanged(false)
    // Work that begins inside the release delay stays covered.
    policy.onHeldChanged(true)
    delays.elapse()
    assertEquals(listOf(true), kept)
    policy.onHeldChanged(false)
    assertEquals(listOf(true), kept)
    delays.elapse()
    assertEquals(listOf(true, false), kept)

    // A hold begun after the page went hidden off screen cannot make it visible again.
    policy.onHeldChanged(true)
    delays.elapse()
    assertEquals(listOf(true, false), kept)
    policy.onResumedChanged(true)
    assertEquals(listOf(true, false, true), kept)
  }

  @Test
  fun `off screen the service outlives the last hold until a moment after the page release so later holds join it`() {
    val delays = Delays()
    val kept = mutableListOf<Boolean>()
    lateinit var policy: PageKeptActivePolicy
    val host = Hosted { policy.onHeldChanged(it) }
    policy = PageKeptActivePolicy({ kept.add(it) }, host::keepService, delays.post, delays.remove)
    policy.onResumedChanged(true)
    assertEquals(BACKGROUND_DEFERRED, host.begin(taskId(1), "generation", deferred = true))
    policy.onResumedChanged(false)
    host.registry.promote(host.start)
    // From here the platform refuses a service start, as it does once the app is in the background.
    host.refuse = BACKGROUND_REFUSED

    assertTrue(host.end(taskId(1)))
    assertEquals(0, host.stops)
    assertEquals(BACKGROUND_ACTIVE, host.begin(taskId(2), "sync", deferred = true))
    assertTrue(host.end(taskId(2)))
    delays.elapse()
    assertEquals(listOf(true, false), kept)
    // A hold a hidden handler begins after the release joins the service instead of being refused.
    assertEquals(BACKGROUND_ACTIVE, host.begin(taskId(3), "sync", deferred = true))
    assertEquals(1, host.starts)
    delays.elapse()
    assertEquals(0, host.stops)
    assertTrue(host.end(taskId(3)))
    assertEquals(1, host.stops)
    assertEquals(listOf(true, false), kept)

    // Back on screen the service stops at once, and a stale keep cannot stop a later service.
    host.refuse = null
    host.registry.demote(host.stop)
    policy.onResumedChanged(true)
    host.begin(taskId(4), "generation", deferred = true)
    policy.onResumedChanged(false)
    host.registry.promote(host.start)
    assertTrue(host.end(taskId(4)))
    delays.elapse()
    host.registry.demote(host.stop)
    policy.onResumedChanged(true)
    assertEquals(2, host.stops)
    host.begin(taskId(5), "generation", deferred = true)
    policy.onResumedChanged(false)
    host.registry.promote(host.start)
    assertTrue(host.end(taskId(5)))
    delays.elapse()
    host.lifecycle.stopAll { true }
    host.begin(taskId(6), "backup")
    delays.elapse()
    assertEquals(2, host.stops)
    assertTrue(host.end(taskId(6)))
    assertEquals(3, host.stops)
  }

  @Test
  fun `a service that never reached the foreground leaves holds registered for the next start`() {
    val host = Hosted()
    host.begin(taskId(1), "backup")
    host.registry.release()
    assertTrue(host.registry.snapshot().isEmpty())
    assertTrue(host.end(taskId(1)))
    assertEquals(0, host.stops)
    host.begin(taskId(2), "backup")
    host.registry.release()
    host.registry.promote(host.start)
    assertEquals(listOf(BackgroundTaskStatus("backup")), host.registry.snapshot())
  }

  @Test
  fun `a hold that starts the service again takes back the holds a failed start left`() {
    val host = Hosted()
    host.begin(taskId(1), "backup")
    host.begin(taskId(2), "sync", deferred = true)
    host.lifecycle.stopAll { true }
    host.registry.release()
    host.begin(taskId(3), "restore")
    // The deferred hold still waits for the app to leave the screen.
    assertEquals(listOf(BackgroundTaskStatus("backup"), BackgroundTaskStatus("restore")), host.registry.snapshot())
    assertTrue(host.end(taskId(3)))
    assertEquals(0, host.stops)
    assertTrue(host.end(taskId(1)))
    assertEquals(1, host.stops)
  }

  @Test
  fun `new documents clear previous holds while repeated readiness preserves current work`() {
    val host = Hosted()
    val ready = AndroidFrontendReady()
    val documents = AndroidDocumentReadyGate()
    fun announce(document: Any): Boolean {
      if (!documents.admit(document)) return false
      return ready.onDocumentReady {
        host.registry.clearAll()
        host.lifecycle.stopAll { host.stops++; true }
      }
    }
    val first = Any()
    val next = Any()
    host.begin(taskId(1), "sync", deferred = true)
    host.begin(taskId(2), "backup")
    assertTrue(announce(first))
    assertTrue(ready.isReady)
    assertTrue(host.registry.snapshot().isEmpty())
    assertEquals(1, host.stops)
    host.registry.promote(host.start)
    host.registry.demote(host.stop)
    host.registry.promote(host.start)
    assertEquals(1, host.starts)
    assertFalse(host.end(taskId(1)))

    host.begin(taskId(3), "sync", deferred = true)
    assertFalse(announce(first))
    assertEquals(2, host.starts)
    assertEquals(1, host.stops)
    assertEquals(listOf(BackgroundTaskStatus("sync")), host.registry.snapshot())

    assertFalse(announce(next))
    assertTrue(ready.isReady)
    assertTrue(host.registry.snapshot().isEmpty())
    assertEquals(2, host.stops)
    assertFalse(host.end(taskId(3)))
    host.begin(taskId(4), "backup")
    assertFalse(announce(first))
    assertFalse(announce(next))
    assertEquals(3, host.starts)
    assertEquals(2, host.stops)
    assertEquals(listOf(BackgroundTaskStatus("backup")), host.registry.snapshot())
    assertTrue(host.end(taskId(4)))
  }

  @Test
  fun `an end from a page that was torn down cannot stop a fresh hold`() {
    val host = Hosted()
    host.begin(taskId(1), "generation")
    host.registry.clearAll()
    host.lifecycle.stopAll { true }
    host.begin(taskId(2), "generation")
    assertFalse(host.end(taskId(1)))
    assertEquals(listOf(BackgroundTaskStatus("generation")), host.registry.snapshot())
    assertTrue(host.end(taskId(2)))
    assertEquals(1, host.stops)
  }

  @Test
  fun `notification groups kinds beside generation with a mean percent and a kind icon and shows user-started work at once`() {
    assertEquals(
      BackgroundNotificationContent(true, emptyList(), -1, BackgroundNotificationIcon.CHAT, immediate = false),
      backgroundNotificationContent(listOf(BackgroundTaskStatus("generation"))),
    )
    assertEquals(
      BackgroundNotificationContent(
        generation = true,
        lines = listOf(BackgroundNotificationLine("backup", 30), BackgroundNotificationLine("export", 90)),
        percent = 50,
        icon = BackgroundNotificationIcon.UPLOAD,
        immediate = true,
      ),
      backgroundNotificationContent(listOf(
        BackgroundTaskStatus("backup", 20),
        BackgroundTaskStatus("generation"),
        BackgroundTaskStatus("export", 90, userInitiated = true),
        BackgroundTaskStatus("backup", 40),
      )),
    )
    assertEquals(
      BackgroundNotificationContent(
        generation = false,
        lines = listOf(BackgroundNotificationLine("restore", 10), BackgroundNotificationLine("sync", -1)),
        percent = -1,
        icon = BackgroundNotificationIcon.SYNC,
        immediate = false,
      ),
      backgroundNotificationContent(listOf(BackgroundTaskStatus("restore", 10), BackgroundTaskStatus("sync"))),
    )
    assertEquals(
      BackgroundNotificationIcon.DOWNLOAD,
      backgroundNotificationContent(listOf(BackgroundTaskStatus("import"), BackgroundTaskStatus("restore"))).icon,
    )
  }

  @Test
  fun `a stop before the start reaches the service leaves stopping to that start`() {
    val lifecycle = GenerationForegroundLifecycle()
    var token = -1L
    val stopped = mutableListOf<Boolean>()
    assertTrue(lifecycle.begin { token = it; true })
    assertTrue(lifecycle.end { stopped.add(it); true })
    assertTrue(lifecycle.begin { token = it; true })
    assertTrue(lifecycle.activate(token, 1, {}, {}))
    assertTrue(lifecycle.stopAll { stopped.add(it); true })
    assertEquals(listOf(false, true), stopped)
  }

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
