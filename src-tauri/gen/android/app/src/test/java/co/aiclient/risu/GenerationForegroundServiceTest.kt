package co.aiclient.risu

import android.app.Service
import org.junit.Assert.assertEquals
import org.junit.Test

class GenerationForegroundServiceTest {
  @Test
  fun `late end does not dispatch after the final generation ended`() {
    val gate = GenerationForegroundDispatchGate()
    var starts = 0
    var ends = 0

    assertEquals(true, gate.begin { starts += 1; true })
    assertEquals(true, gate.end { ends += 1; true })
    assertEquals(false, gate.end { ends += 1; true })

    assertEquals(1, starts)
    assertEquals(1, ends)
  }

  @Test
  fun `dispatch gate preserves nested generations and resets after timeout`() {
    val gate = GenerationForegroundDispatchGate()
    var ends = 0

    assertEquals(true, gate.begin { true })
    assertEquals(true, gate.begin { true })
    assertEquals(true, gate.end { ends += 1; true })
    gate.timeout()
    assertEquals(false, gate.end { ends += 1; true })

    assertEquals(1, ends)
  }

  @Test
  fun `failed dispatch rolls back the matching generation transition`() {
    val gate = GenerationForegroundDispatchGate()
    var ends = 0

    assertEquals(false, gate.begin { false })
    assertEquals(false, gate.end { ends += 1; true })
    assertEquals(true, gate.begin { true })
    assertEquals(false, gate.end { ends += 1; false })
    assertEquals(true, gate.end { ends += 1; true })

    assertEquals(2, ends)
  }

  @Test
  fun `first begin starts and nested begin does not start twice`() {
    val controller = GenerationForegroundController()

    assertEquals(GenerationForegroundCommand.START, controller.begin(notificationsEnabled = true))
    assertEquals(GenerationForegroundCommand.NONE, controller.begin(notificationsEnabled = true))
  }

  @Test
  fun `only final end stops and extra end stays stopped`() {
    val controller = GenerationForegroundController()
    controller.begin(notificationsEnabled = true)
    controller.begin(notificationsEnabled = true)

    assertEquals(GenerationForegroundCommand.NONE, controller.end())
    assertEquals(GenerationForegroundCommand.STOP, controller.end())
    assertEquals(GenerationForegroundCommand.NONE, controller.end())
  }

  @Test
  fun `denied notification begin is a no-op`() {
    val controller = GenerationForegroundController()

    assertEquals(GenerationForegroundCommand.NONE, controller.begin(notificationsEnabled = false))
    assertEquals(GenerationForegroundCommand.NONE, controller.end())
  }

  @Test
  fun `timeout stops and clears the refcount`() {
    val controller = GenerationForegroundController()
    controller.begin(notificationsEnabled = true)
    controller.begin(notificationsEnabled = true)

    assertEquals(GenerationForegroundCommand.STOP, controller.timeout())
    assertEquals(GenerationForegroundCommand.NONE, controller.end())
  }

  @Test
  fun `service is not sticky`() {
    assertEquals(Service.START_NOT_STICKY, GENERATION_FOREGROUND_START_MODE)
  }
}
