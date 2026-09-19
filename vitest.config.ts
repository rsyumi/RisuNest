import { defineConfig, mergeConfig } from 'vitest/config'
import { sharedVitestConfig } from './tests/vitestShared'
import { appTestIncludes, extendedAppTests, harnessVitestTests, separateRunnerPaths } from './tests/suiteOwnership.mjs'

export default mergeConfig(sharedVitestConfig(), defineConfig({
  test: {
    coverage: {
      provider: 'v8',
      enabled: false,
      include: ['src/**/*.{ts,svelte}'],
      exclude: [
        'src/**/*.d.ts',
        'src/**/*.{test,spec}.?(c|m)[jt]s?(x)',
        'src/**/*.test.svelte',
        'src/**/*.test.svelte.ts',
        'src/**/*.bench.ts',
        'src/**/tests/**',
        'src/**/{__tests__,__fixtures__}/**',
        'src/**/test-fixtures/**',
        'src/**/*.testSupport.ts',
        'src/**/*.testUtils.ts',
        'src/lib/ChatScreens/chatMountProbe.ts',
        'src/ts/process/luaWorkerPilotClient.ts',
        'src/ts/realmEndpoints.blocked.ts',
      ],
      reporter: ['text-summary', 'json-summary', 'lcov', 'html'],
      reportsDirectory: '.tmp/test-results/coverage',
      reportOnFailure: true,
    },
    projects: [
      {
        extends: true,
        test: {
          name: 'app',
          include: appTestIncludes,
          exclude: [...extendedAppTests, ...harnessVitestTests, ...separateRunnerPaths],
        },
      },
      {
        extends: true,
        test: { name: 'app-extended', include: extendedAppTests },
      },
      {
        extends: true,
        test: { name: 'harness', include: harnessVitestTests, exclude: separateRunnerPaths },
      },
    ],
  },
}))
