import { defineConfig, type Plugin } from "vite";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { svelte, vitePreprocess } from "@sveltejs/vite-plugin-svelte";
import wasm from "vite-plugin-wasm";
import strip from '@rollup/plugin-strip';
import tailwindcss from '@tailwindcss/vite'
// Swaps the RisuRealm endpoint module for an unresolvable-address version, only
// under `--mode agent`.
//
// When an agent runs the dev server or a built app and looks at the screen,
// third-party Realm content we do not control would otherwise reach its context
// through screenshots and page reads. A rule such as "do not open the Realm tab"
// cannot un-render a screen, so the build enforces it instead.
//
// Imports mix the relative path (`./realmEndpoints`) with the alias path
// (`src/ts/realmEndpoints`), so a string alias does not catch them all. Match on
// the resolved file path.
const realmEndpointBlockPlugin: Plugin = {
  name: 'risunest-block-realm-endpoints',
  enforce: 'pre',
  resolveId: {
    // rolldown crosses the native boundary on every JS hook call. The filter
    // restricts this hook to imports whose name is a candidate, so the tens of
    // thousands of others cost nothing. `.blocked.ts` does not match the filter,
    // so it is never swapped back.
    filter: { id: /realmEndpoints(\.ts)?$/ },
    async handler(source, importer, options) {
      const resolved = await this.resolve(source, importer, { ...options, skipSelf: true })
      if (!resolved) return null
      const normalized = resolved.id.replace(/\\/g, '/')
      if (!normalized.endsWith('/src/ts/realmEndpoints.ts')) return null
      return resolved.id.replace(/realmEndpoints\.ts$/, 'realmEndpoints.blocked.ts')
    },
  },
}

const startupAppearancePlugin: Plugin = {
  name: 'risunest-startup-appearance',
  transformIndexHtml: {
    order: 'post',
    handler(html) {
      if (!html.includes('id="preloading"')) return;
      return [{
        tag: 'script',
        children: readFileSync(new URL('./src-tauri/src/windows_appearance/startup.js', import.meta.url), 'utf8'),
        injectTo: 'head-prepend',
      }];
    },
  },
}

// https://vitejs.dev/config/
export default defineConfig(({command, mode}) => {
  // `pnpm tauribuild:android` runs with `--mode android`, and `tauri android
  // build` additionally exports TAURI_ENV_PLATFORM=android. Either signal means
  // the bundle is about to be embedded into the Android cdylib, where source
  // maps are dead weight: they cannot be opened on device and they inflate the
  // APK by tens of megabytes. Desktop and web builds are untouched.
  const isAndroidBundle = mode === 'android' || process.env.TAURI_ENV_PLATFORM === 'android'
  // The Android check also reads TAURI_ENV_PLATFORM, so building Android with
  // `--mode agent` keeps the existing Android handling such as source-map
  // exclusion.
  //
  // Vite only loads `.env.<mode>` though. `--mode agent` loads neither
  // `.env.desktop` nor `.env.android`, so `.env.agent` has to carry what those
  // two files carry. Change them together.
  const blockRealmEndpoints = mode === 'agent'
  const stripBuildOutput = () => strip({
    include: '**/*.(mjs|js|svelte|ts)',
    functions: [
      'assert.*',
      ...(process.env.TAURI_ENV_DEBUG === 'true' ? [] : [
        'console.log', 'console.info', 'console.debug', 'console.trace',
        'console.table', 'console.dir', 'console.dirxml',
        'console.group', 'console.groupCollapsed', 'console.groupEnd',
        'console.time', 'console.timeLog', 'console.timeEnd',
        'console.count', 'console.countReset', 'console.profile',
        'console.profileEnd', 'console.timeStamp',
      ]),
    ],
  })
  return {
    plugins: [
      blockRealmEndpoints ? realmEndpointBlockPlugin : null,
      startupAppearancePlugin,
      svelte({
        preprocess: vitePreprocess(),
        onwarn: (warning, handler) => {
          // disable a11y warnings
          if (warning.code.startsWith("a11y-")) return;
          handler(warning);
        },
        compilerOptions: {
          experimental: {
            async: true
          }
        }
      }),
      tailwindcss(),
      wasm(),
      command === 'build' ? stripBuildOutput() : null
    ],

    // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
    // prevent vite from obscuring rust errors
    clearScreen: false,
    // tauri expects a fixed port, fail if that port is not available
    server: {
      host: '0.0.0.0', // listen on all addresses
      port: 5174,
      strictPort: true,
      // hmr: false,
      // The watcher crawls everything under the root except node_modules and
      // .git. The shared Cargo target, the generated Android project with its
      // Gradle output, local scratch output in .tmp and worktree checkouts add
      // about 100k directories, enough to push the dev server past the default
      // heap limit before the crawl ends. None of them hold app sources.
      watch: {
        ignored: [
          '**/src-tauri/target/**',
          '**/src-tauri/gen/**',
          '**/.tmp/**',
          '**/.worktrees/**',
          '**/.claude/worktrees/**',
        ],
      },
    },
    // to make use of `TAURI_ENV_DEBUG` and other env variables
    // https://v2.tauri.app/reference/environment-variables/
    envPrefix: ["VITE_", "TAURI_"],
    build: {
      target:'baseline-widely-available',
      reportCompressedSize: false,
      // don't minify for debug builds
      minify: process.env.TAURI_ENV_DEBUG === 'true' ? false : 'oxc',
      // Desktop release errors must remain translatable offline. Android excludes maps.
      sourcemap: !isAndroidBundle && (mode === 'desktop' || process.env.TAURI_ENV_PLATFORM === 'windows' || process.env.TAURI_ENV_DEBUG === 'true'),
      chunkSizeWarningLimit: 2000,
    },
    
    optimizeDeps:{
      // Without this the dependency scan treats every HTML file under the root
      // as an entry, including Tauri codegen copies of built pages and Cargo
      // and Gradle reports in the build trees. Other HTML pages are still served;
      // their dependencies are discovered on first request instead.
      entries: ['index.html'],
      exclude: [
        "@browsermt/bergamot-translator"
      ],
      needsInterop:[
        "@mlc-ai/web-tokenizers"
      ]
    },

    resolve:{
      alias: [
        { find: /^monaco-editor$/, replacement: fileURLToPath(new URL('./src/ts/gui/codearea/monacoRuntime.ts', import.meta.url)) },
        { find: 'src', replacement: '/src' },
      ]
    },
    worker: {
      format: 'es',
      plugins: () => command === 'build' ? [stripBuildOutput()] : [],
    }
}
});
