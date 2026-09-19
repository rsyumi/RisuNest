import { defineConfig, mergeConfig } from 'vitest/config'
import { sharedVitestConfig } from './tests/vitestShared'
import { appTestIncludes, extendedAppTests, harnessVitestTests, separateRunnerPaths } from './tests/suiteOwnership.mjs'

export default mergeConfig(sharedVitestConfig(), defineConfig({
  test: {
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
