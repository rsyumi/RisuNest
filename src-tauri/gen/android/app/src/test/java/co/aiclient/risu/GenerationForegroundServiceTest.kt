package co.aiclient.risu

import android.app.Service
import org.junit.Assert.assertEquals
import org.junit.Test

class GenerationForegroundServiceTest {
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
  fun `timeout termination is unconditional after clearing the refcount`() {
    assertEquals(
      GenerationForegroundTermination.UNCONDITIONAL,
      generationForegroundTimeoutTermination(),
    )
  }

  @Test
  fun `service is not sticky`() {
    assertEquals(Service.START_NOT_STICKY, GENERATION_FOREGROUND_START_MODE)
  }
}
