// Android observes build-only call sites because the native bridge is immutable.
export const androidObservation = `(() => {
    window.__startupObserveCall = function(command, invoke) {
        if (!['pds_open','pds_read_root','pds_materialize','pds_commit','pds_replace_commit'].includes(command)) return invoke();
        const entry = {command, start: performance.now(), ms: null, success: false, bytes: null};
        window.__startupMetrics.calls.push(entry);
        window.__startupBegin?.(command);
        let promise;
        try { promise = invoke(); } catch (error) {
            entry.ms = performance.now() - entry.start;
            window.__startupRecord?.(command, entry.start, false);
            throw error;
        }
        promise.then(result => {
            entry.ms = performance.now() - entry.start; entry.success = true;
            window.__startupRecord?.(command, entry.start, true);
            if (command === 'pds_open' && window.__startupMetrics.firstRevision === null)
                window.__startupMetrics.firstRevision = result.revision;
        }, () => {
            entry.ms = performance.now() - entry.start;
            window.__startupRecord?.(command, entry.start, false);
        });
        return promise;
    };
})()`
import { instrumentation } from './cdp.mjs'

export function startupInstrumentation(platform, diagnosticStages = false) {
    const diagnostics = `;(() => {
        const seen = new Set();
        for (const hook of ['__startupBegin', '__startupRecord']) {
            const original = window[hook];
            window[hook] = function(stage, ...args) {
                const key = hook + ':' + stage;
                if (!seen.has(key)) {
                    seen.add(key);
                    void window.__TAURI_INTERNALS__.invoke('native_log_error', {
                        message: 'M0-stage:' + key
                    }).catch(() => {});
                }
                return original.call(this, stage, ...args);
            };
        }
    })()`
    return (
        instrumentation +
        ';' +
        (platform === 'android' ? androidObservation : '') +
        (diagnosticStages ? diagnostics : '')
    )
}
