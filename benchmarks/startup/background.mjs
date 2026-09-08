import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdtemp, readFile, writeFile } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

// Keep the complete process tree on a private desktop. Hiding a PowerShell
// console alone does not stop a native GUI application from showing its window.
export async function runOnPrivateDesktop(args, output) {
    const directory = await mkdtemp(path.join(os.tmpdir(), 'risunest-startup-runner-'))
    const entry = path.join(directory, 'entry.mjs')
    const log = path.resolve(output + '.runner.log')
    const profile = fileURLToPath(new URL('./profile.mjs', import.meta.url))
    await writeFile(
        entry,
        `
        import {spawnSync} from 'node:child_process';
        import {openSync,closeSync} from 'node:fs';
        const log = openSync(${JSON.stringify(log)}, 'w');
        const result = spawnSync(process.execPath, ${JSON.stringify([profile, ...args])}, {
            stdio: ['ignore', log, log], windowsHide: true,
        });
        closeSync(log);
        process.exitCode = result.status ?? 1;
    `,
    )
    const child = spawn(
        'powershell.exe',
        [
            '-NoProfile',
            '-NonInteractive',
            '-File',
            fileURLToPath(new URL('./private-desktop.ps1', import.meta.url)),
            '-EntryScript',
            entry,
        ],
        { windowsHide: true, stdio: 'inherit' },
    )
    const [code] = await once(child, 'exit')
    try {
        process.stdout.write(await readFile(log, 'utf8'))
    } catch {
        /* No fallback to the input desktop. */
    }
    if (code !== 0) throw new Error('Private desktop benchmark failed')
}
