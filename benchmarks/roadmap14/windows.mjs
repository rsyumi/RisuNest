import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { once } from 'node:events'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath, pathToFileURL } from 'node:url'

import { createRoadmap14Result } from './result.mjs'
import { getRoadmap14Scenario } from './scenarios.mjs'

const EXPORT_DIGEST_PROVENANCE = 'sha256-of-u64le-length-prefixed-json-fragments-in-export-traversal-order-after-append'

export function convertExistingWindowsMeasurements({
    phase3,
    tauri,
    sourceRevision,
    artifactHashes,
    platformIdentity,
    appVersion,
    recordedAt,
    osVersion,
    architecture,
}) {
    if (phase3?.benchmark !== 'phase3-step5-persistent-store') {
        throw new Error('Expected the Phase 3 save-large persistent-store result')
    }
    if (phase3.schemaVersion !== 1 || tauri?.schemaVersion !== 1) {
        throw new Error('Expected schema version 1 from both existing measurements')
    }
    if (tauri.build?.release !== true || tauri.build?.realmDisabled !== true) {
        throw new Error('Windows conversion requires a Realm-disabled release Tauri measurement')
    }
    if (!Array.isArray(phase3.samples) || phase3.samples.length === 0) {
        throw new Error('Phase 3 result has no retained samples')
    }
    if (phase3.exportTraversalSha256Provenance !== EXPORT_DIGEST_PROVENANCE) {
        throw new Error('Phase 3 result has unknown export traversal SHA-256 provenance')
    }
    if (
        !sourceRevision
        || phase3.sourceRevision !== sourceRevision
        || tauri.build?.sourceRevision !== sourceRevision
        || !tauri.build?.identifier?.includes(`.r${sourceRevision.slice(0, 12)}.`)
    ) {
        throw new Error('Windows measurement revisions and embedded Tauri identifier must match Git HEAD')
    }

    const descriptor = getRoadmap14Scenario('save-large').descriptor
    const expectedShape = {
        characters: descriptor.characters,
        totalConversations: descriptor.characters * descriptor.chatsPerCharacter + 1,
        totalMessages: descriptor.characters * descriptor.chatsPerCharacter
            * descriptor.turnsPerChat + descriptor.stressChat.turns,
    }
    if (
        tauri.fixture?.kind !== 'phase3-step5-save-large'
        || tauri.fixture.serializedSha256 !== phase3.fixture?.sha256
        || tauri.fixture.serializedBytes !== phase3.fixture?.serializedBytes
        || Object.entries(expectedShape).some(([name, value]) => (
            phase3.fixture?.[name] !== value || tauri.fixture?.[name] !== value
        ))
    ) {
        throw new Error('Windows measurements must use the same frozen save-large fixture')
    }

    const canonicalHashes = new Set(
        phase3.samples.map((sample) => sample.exportTraversalSha256),
    )
    if (canonicalHashes.size !== 1 || !/^[0-9a-f]{64}$/.test([...canonicalHashes][0] ?? '')) {
        throw new Error('Phase 3 samples must agree on the exported canonical output SHA-256')
    }

    const memorySamples = [
        ...(tauri.explicitImport?.memorySamples ?? []),
        ...(tauri.snapshot?.memorySamples ?? []),
    ].filter(Boolean)
    const lastSample = phase3.samples.at(-1)

    return createRoadmap14Result({
        scenario: 'save-large',
        status: 'completed',
        recordedAt,
        build: {
            identity: `${sourceRevision}-windows-release`,
            sourceRevision,
            profile: 'release',
            target: 'x86_64-pc-windows-msvc',
            appVersion,
            realmDisabled: true,
        },
        platform: {
            family: 'windows',
            identity: platformIdentity,
            osVersion,
            architecture,
            webViewVersion: tauri.platform?.webViewUserAgent ?? null,
            deviceModel: null,
        },
        memory: {
            measurement: 'js-heap-and-rss',
            heapUsedBytes: memorySamples.map((sample) => sample.jsHeap.usedBytes),
            rssBytes: memorySamples.map((sample) => sample.processMemory.workingSetBytes),
            pssBytes: [],
        },
        ui: {
            domNodeCount: tauri.ui?.domNodeCount ?? null,
            mountedMessageCount: tauri.ui?.mountedMessageCount ?? null,
            liveUrlCount: tauri.ui?.liveUrlCount ?? null,
        },
        latency: {
            samples: [
                {
                    name: 'staged-replace-import',
                    valuesMs: phase3.samples.map((sample) => sample.importUs / 1000),
                },
                {
                    name: 'append-commit',
                    valuesMs: phase3.samples.map((sample) => sample.appendCommitUs / 1000),
                },
                {
                    name: 'export-materialize-and-traversal-total',
                    valuesMs: phase3.samples.map((sample) => sample.exportTotalUs / 1000),
                },
                {
                    name: 'snapshot-create',
                    valuesMs: phase3.samples.map((sample) => sample.snapshotUs / 1000),
                },
            ],
        },
        bytes: {
            artifacts: [
                { name: 'fixture-serialized-json', bytes: phase3.fixture.serializedBytes },
                { name: 'export-traversal-json-fragments', bytes: lastSample.exportTraversalJsonBytes },
                { name: 'snapshot-file', bytes: lastSample.snapshotBytes },
            ],
        },
        canonicalOutput: {
            sha256: [...canonicalHashes][0],
            provenance: EXPORT_DIGEST_PROVENANCE,
        },
        source: {
            runner: 'roadmap14-windows-v2',
            measurements: [
                'persistent-store-staged-replace-import',
                'persistent-store-append-commit',
                'persistent-store-export-materialize-and-framed-traversal-total',
                'persistent-store-snapshot-create',
                'tauri-save-large-staged-import-memory',
                'tauri-post-stage-shell-ui',
            ],
            artifacts: [
                { name: 'save-large-fixture-json', sha256: phase3.fixture.sha256 },
                { name: 'phase3-result-json', sha256: artifactHashes.phase3Result },
                { name: 'tauri-cdp-result-json', sha256: artifactHashes.tauriResult },
            ],
        },
        notes: [
            'Phase 3 save-large timings are native SQLite measurements in a release Rust test process.',
            'Heap and RSS samples cover the exact fixture staged import and post-stage snapshot in isolated release Tauri.',
            'The Tauri staged import uses the same serialized save-large fixture as the Rust measurement.',
            'UI counts describe the application shell after staging, not 510,000 simultaneously rendered messages.',
            'Live RisuRealm is intentionally not exercised.',
        ],
    })
}

export function parseArguments(argumentsList) {
    const options = {
        phase3Result: null,
        tauriResult: null,
        output: null,
        runExisting: false,
        rawOutputDirectory: null,
        platformIdentity: null,
        recordedAt: null,
    }
    for (let index = 0; index < argumentsList.length; index += 1) {
        const argument = argumentsList[index]
        if (argument === '--phase3-result') {
            options.phase3Result = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--tauri-result') {
            options.tauriResult = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--output') {
            options.output = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--run-existing') options.runExisting = true
        else if (argument === '--raw-output-dir') {
            options.rawOutputDirectory = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--platform-identity') {
            options.platformIdentity = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--recorded-at') {
            options.recordedAt = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--help' || argument === '-h') options.help = true
        else throw new Error(`Unknown argument: ${argument}`)
    }
    return options
}

function requiredValue(argumentsList, index, option) {
    const value = argumentsList[index]
    if (!value || value.startsWith('--')) throw new Error(`${option} requires a value`)
    return value
}

async function runCommand(command, args, options) {
    const child = spawn(command, args, {
        cwd: options.cwd,
        env: options.env,
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe'],
    })
    child.stdout.on('data', (chunk) => process.stderr.write(chunk))
    child.stderr.on('data', (chunk) => process.stderr.write(chunk))
    const [exitCode] = await once(child, 'exit')
    if (exitCode !== 0) throw new Error(`${command} exited with code ${exitCode}`)
}

async function captureCommand(command, args, cwd) {
    const child = spawn(command, args, {
        cwd,
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', (chunk) => { stdout += chunk })
    child.stderr.on('data', (chunk) => { stderr += chunk })
    const [exitCode] = await once(child, 'exit')
    if (exitCode !== 0) throw new Error(`${command} failed: ${stderr.trim()}`)
    return stdout.trim()
}

async function runExistingMeasurements(repositoryRoot, rawOutputDirectory, sourceRevision) {
    if (process.platform !== 'win32') throw new Error('The existing release runner supports Windows only')
    await mkdir(rawOutputDirectory, { recursive: true })
    const phase3Result = path.join(rawOutputDirectory, 'phase3-save-large.json')
    const phase3Fixture = path.join(rawOutputDirectory, 'phase3-save-large-fixture.json')
    const tauriResult = path.join(rawOutputDirectory, 'phase3-tauri-cdp.json')
    const environment = {
        ...process.env,
        RISUNEST_PHASE3_BENCH_OUTPUT: phase3Result,
        RISUNEST_PHASE3_FIXTURE_OUTPUT: phase3Fixture,
        RISUNEST_PHASE3_BENCH_REVISION: sourceRevision,
    }
    await runCommand(
        'cargo.exe',
        [
            'test',
            '--manifest-path',
            path.join(repositoryRoot, 'src-tauri', 'Cargo.toml'),
            '--release',
            '--locked',
            '--lib',
            'persistent_store::benchmark::phase3_step5_measurements',
            '--',
            '--ignored',
            '--exact',
            '--nocapture',
            '--test-threads=1',
        ],
        { cwd: repositoryRoot, env: environment },
    )
    await runCommand(
        process.execPath,
        [
            path.join(repositoryRoot, 'benchmarks', 'phase3', 'tauri-cdp.mjs'),
            '--save-large-fixture',
            phase3Fixture,
            '--output',
            tauriResult,
        ],
        { cwd: repositoryRoot, env: environment },
    )
    return { phase3Result, tauriResult }
}

function usage() {
    return [
        'Usage: node benchmarks/roadmap14/windows.mjs [options]',
        '',
        'Options:',
        '  --run-existing                    Run the existing Phase 3 Rust and Tauri CDP measurements.',
        '  --phase3-result <path>             Reuse a Phase 3 save-large JSON result.',
        '  --tauri-result <path>              Reuse a Phase 3 Tauri CDP JSON result.',
        '  --platform-identity <value>        Optional stable reference-host identity.',
        '  --recorded-at <ISO date>           Optional stable timestamp.',
        '  --raw-output-dir <path>            Existing measurement output directory.',
        '  --output <path>                    Also write the shared JSON result.',
        '  -h, --help                         Show this help.',
    ].join(os.EOL)
}

async function main() {
    const options = parseArguments(process.argv.slice(2))
    if (options.help) {
        process.stdout.write(`${usage()}${os.EOL}`)
        return
    }
    const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
    const sourceRevision = await captureCommand('git.exe', ['rev-parse', 'HEAD'], repositoryRoot)
    let phase3Result = options.phase3Result
    let tauriResult = options.tauriResult
    if (options.runExisting) {
        const rawOutputDirectory = path.resolve(
            options.rawOutputDirectory
                ?? path.join(repositoryRoot, 'src-tauri', 'target', 'roadmap14-baseline'),
        )
        const paths = await runExistingMeasurements(repositoryRoot, rawOutputDirectory, sourceRevision)
        phase3Result = paths.phase3Result
        tauriResult = paths.tauriResult
    }
    if (!phase3Result || !tauriResult) {
        throw new Error('Provide --run-existing or both --phase3-result and --tauri-result')
    }
    const [phase3Artifact, tauriArtifact, packageJson] = await Promise.all([
        readArtifact(phase3Result),
        readArtifact(tauriResult),
        readJson(path.join(repositoryRoot, 'package.json')),
    ])
    const result = convertExistingWindowsMeasurements({
        phase3: phase3Artifact.value,
        tauri: tauriArtifact.value,
        sourceRevision,
        artifactHashes: {
            phase3Result: phase3Artifact.sha256,
            tauriResult: tauriArtifact.sha256,
        },
        platformIdentity: options.platformIdentity ?? `windows-${os.arch()}-${os.release()}`,
        appVersion: packageJson.version,
        recordedAt: options.recordedAt ?? new Date().toISOString(),
        osVersion: `${os.type()} ${os.release()}`,
        architecture: os.arch(),
    })
    const json = `${JSON.stringify(result, null, 2)}${os.EOL}`
    if (options.output) {
        const outputPath = path.resolve(options.output)
        await mkdir(path.dirname(outputPath), { recursive: true })
        await writeFile(outputPath, json, 'utf8')
    }
    process.stdout.write(json)
}

async function readJson(filePath) {
    return JSON.parse(await readFile(path.resolve(filePath), 'utf8'))
}

async function readArtifact(filePath) {
    const bytes = await readFile(path.resolve(filePath))
    return {
        value: JSON.parse(bytes.toString('utf8')),
        sha256: createHash('sha256').update(bytes).digest('hex'),
    }
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
    main().catch((error) => {
        process.stderr.write(`${error.stack ?? error}${os.EOL}`)
        process.exitCode = 1
    })
}
