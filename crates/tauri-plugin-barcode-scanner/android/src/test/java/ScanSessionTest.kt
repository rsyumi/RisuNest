package app.tauri.barcodescanner
import java.io.File
import org.junit.Assert.*
import org.junit.Test
class ScanSessionTest {
    @Test fun cancellation_returns_callback_once_before_cleanup() {
        val session = ScanSession<Any>(); val callback = Any()
        assertTrue(session.begin(callback))
        assertSame(callback, session.finish())
        assertNull(session.pending)
        assertNull(session.finish())
    }
    @Test fun late_readiness_and_decode_cannot_affect_a_new_scan() {
        val session = ScanSession<Any>()
        assertTrue(session.begin(Any())); val old = session.generation
        session.finish(); assertFalse(session.isCurrent(old))
        assertTrue(session.begin(Any())); assertFalse(session.isCurrent(old))
        assertTrue(session.isCurrent(session.generation))
    }
    @Test fun another_start_does_not_replace_the_existing_callback() {
        val session = ScanSession<Any>(); val first = Any()
        assertTrue(session.begin(first)); assertFalse(session.begin(Any()))
        assertSame(first,session.finish())
    }
    @Test fun one_frame_failure_does_not_end_the_scan_session() {
        val session = ScanSession<Any>()
        assertTrue(session.begin(Any()))
        val generation = session.generation
        assertFalse(session.shouldFailAfterFrameError(generation))
        assertTrue(session.isCurrent(generation))
        session.recordFrameSuccess(generation)
        assertFalse(session.shouldFailAfterFrameError(generation))
    }
    @Test fun repeated_frame_failures_end_the_current_scan_only() {
        val session = ScanSession<Any>()
        assertTrue(session.begin(Any()))
        val generation = session.generation
        assertFalse(session.shouldFailAfterFrameError(generation))
        assertFalse(session.shouldFailAfterFrameError(generation))
        assertTrue(session.shouldFailAfterFrameError(generation))
        session.finish()
        assertFalse(session.shouldFailAfterFrameError(generation))
    }
    @Test fun lifecycle_observer_cancels_on_stop() {
        var cancellations = 0
        val observer = ScanLifecycleObserver { cancellations++ }
        observer.onHostStop()
        assertEquals(1, cancellations)
    }
    @Test fun camera_permission_does_not_make_camera_hardware_required() {
        val manifest = File("src/main/AndroidManifest.xml").readText()
        for (feature in listOf(
            "android.hardware.camera.any",
            "android.hardware.camera",
            "android.hardware.camera.autofocus",
        )) {
            assertTrue(manifest.contains("android:name=\"$feature\" android:required=\"false\""))
        }
    }
}
