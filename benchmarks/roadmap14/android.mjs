import { mkdir, readFile, writeFile } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

import { createRoadmap14Result } from './result.mjs'
import { listRoadmap14Scenarios } from './scenarios.mjs'
import { validateRoadmap14Result } from './result-schema.mjs'

export function createPendingAndroidResults({ sourceRevision, appVersion, recordedAt }) {
    return listRoadmap14Scenarios().map(({ name }) =>
        createRoadmap14Result({
            scenario: name,
            status: 'pending',
            recordedAt,
            build: {
                identity: `${sourceRevision ?? 'unknown'}-android-pending`,
                sourceRevision: sourceRevision ?? null,
                profile: 'release',
                target: 'android-physical-device',
                appVersion,
                realmDisabled: true,
            },
            platform: {
                family: 'android',
                identity: 'physical-device-pending',
                osVersion: 'pending',
                architecture: 'pending',
                webViewVersion: null,
                deviceModel: null,
            },
            memory: {
                measurement: 'pending',
                heapUsedBytes: [],
                rssBytes: [],
                pssBytes: [],
            },
            ui: {
                domNodeCount: null,
                mountedMessageCount: null,
                liveUrlCount: null,
            },
            latency: {
                samples: [],
            },
            bytes: {
                artifacts: [],
            },
            canonicalOutput: null,
            source: {
                runner: 'roadmap14-android-v2',
                measurements: [],
                artifacts: [],
            },
            notes: [
                'Physical Android device measurement is pending.',
                'No emulator result is substituted as performance evidence.',
                'Live RisuRealm is intentionally not exercised.',
            ],
        }),
    )
}

export function validateAndroidInstrumentationResults(results) {
    if (!Array.isArray(results)) throw new Error('Android instrumentation output must be an array')
    const expectedNames = listRoadmap14Scenarios().map(({ name }) => name)
    const actualNames = results.map(({ scenario }) => scenario)
    if (
        actualNames.length !== expectedNames.length
        || new Set(actualNames).size !== expectedNames.length
        || expectedNames.some((name) => !actualNames.includes(name))
    ) {
        throw new Error('Android instrumentation must emit exactly one result for each scenario')
    }
    results.forEach((result, index) => {
        const errors = validateRoadmap14Result(result)
        if (errors.length > 0) {
            throw new Error(`Invalid Android instrumentation result ${index}:\n${errors.join('\n')}`)
        }
        if (result.platform.family !== 'android') {
            throw new Error(`Android instrumentation result ${index} must use platform.family android`)
        }
        if (result.status !== 'completed') {
            throw new Error(`Android instrumentation result ${index} must have status completed`)
        }
        const physicalIdentity = [result.platform.identity, result.platform.deviceModel]
            .filter(Boolean)
            .join(' ')
        if (
            result.build.target !== 'android-physical-device'
            || result.platform.deviceModel === null
            || /(?:emulator|avd|sdk_gphone|pending)/i.test(physicalIdentity)
        ) {
            throw new Error(`Android instrumentation result ${index} must identify a physical device`)
        }
    })
    const deviceFields = [
        'family',
        'identity',
        'osVersion',
        'architecture',
        'webViewVersion',
        'deviceModel',
    ]
    if (results.some((result) => !sameFields(result.platform, results[0].platform, deviceFields))) {
        throw new Error('Android instrumentation results must come from one physical device')
    }
    const buildFields = [
        'identity',
        'sourceRevision',
        'profile',
        'target',
        'appVersion',
        'realmDisabled',
    ]
    if (
        !/^[0-9a-f]{40}$/.test(results[0].build.sourceRevision ?? '')
        || results.some((result) => !sameFields(result.build, results[0].build, buildFields))
    ) {
        throw new Error('Android instrumentation results must use one build and source revision')
    }
    return results
}

function sameFields(left, right, fields) {
    return fields.every((name) => left[name] === right[name])
}

function parseArguments(argumentsList) {
    const options = {
        output: null,
        measurement: null,
        sourceRevision: null,
        appVersion: '1.0.0',
        recordedAt: new Date().toISOString(),
    }
    for (let index = 0; index < argumentsList.length; index += 1) {
        const argument = argumentsList[index]
        if (argument === '--output') options.output = requiredValue(argumentsList, ++index, argument)
        else if (argument === '--measurement') {
            options.measurement = requiredValue(argumentsList, ++index, argument)
        }
        else if (argument === '--source-revision') {
            options.sourceRevision = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--app-version') {
            options.appVersion = requiredValue(argumentsList, ++index, argument)
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

function usage() {
    return [
        'Usage: node benchmarks/roadmap14/android.mjs [options]',
        '',
        'Writes schema-valid pending records for the four physical-device scenarios.',
        'Instrumentation must replace pending fields with measurements from one real device run.',
        '',
        'Options:',
        '  --measurement <path>       Validate physical-device instrumentation JSON.',
        '  --output <path>            Also write the JSON array to this path.',
        '  --source-revision <value>  Source revision recorded in build identity.',
        '  --app-version <value>      Application version, default 1.0.0.',
        '  --recorded-at <ISO date>   Stable timestamp for reproducible contract tests.',
        '  -h, --help                 Show this help.',
    ].join(os.EOL)
}

async function main() {
    const options = parseArguments(process.argv.slice(2))
    if (options.help) {
        process.stdout.write(`${usage()}${os.EOL}`)
        return
    }
    const results = options.measurement
        ? validateAndroidInstrumentationResults(
            JSON.parse(await readFile(path.resolve(options.measurement), 'utf8')),
        )
        : createPendingAndroidResults(options)
    const json = `${JSON.stringify(results, null, 2)}${os.EOL}`
    if (options.output) {
        const outputPath = path.resolve(options.output)
        await mkdir(path.dirname(outputPath), { recursive: true })
        await writeFile(outputPath, json, 'utf8')
    }
    process.stdout.write(json)
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
    main().catch((error) => {
        process.stderr.write(`${error.stack ?? error}${os.EOL}`)
        process.exitCode = 1
    })
}
