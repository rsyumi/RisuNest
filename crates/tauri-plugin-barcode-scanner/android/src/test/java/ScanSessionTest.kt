package app.tauri.barcodescanner
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
}
