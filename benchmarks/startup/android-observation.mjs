// Android invokes native IPC directly, without the Windows fetch transport.
export const androidObservation = `(() => {
    const native = window.__TAURI_INTERNALS__;
    if (!native?.invoke) return;
    const original = native.invoke;
    native.invoke = function(command, args, options) {
        const promise = original.call(this, command, args, options);
        if (!['pds_open','pds_commit','pds_replace_commit'].includes(command)) return promise;
        const entry = {command, start: performance.now(), ms: null, success: false, bytes: null};
        window.__startupMetrics.calls.push(entry);
        promise.then(result => {
            entry.ms = performance.now() - entry.start; entry.success = true;
            if (command === 'pds_open' && window.__startupMetrics.firstRevision === null)
                window.__startupMetrics.firstRevision = result.revision;
        }, () => {entry.ms = performance.now() - entry.start;});
        return promise;
    };
})()`
import { instrumentation } from './cdp.mjs'

export function startupInstrumentation(platform) {
    return instrumentation + ';' + (platform === 'android' ? androidObservation : '')
}
