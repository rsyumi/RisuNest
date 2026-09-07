import { defineConfig, type Plugin } from "vite";
import { svelte, vitePreprocess } from "@sveltejs/vite-plugin-svelte";
import wasm from "vite-plugin-wasm";
import strip from '@rollup/plugin-strip';
import tailwindcss from '@tailwindcss/vite'
// `--mode agent`에서만 RisuRealm 엔드포인트 모듈을 해석 불가 주소 버전으로 갈아끼운다.
//
// AI가 dev server나 빌드한 앱을 띄우고 화면을 확인할 때, 우리가 관리하지 않는 제3자
// Realm 콘텐츠가 스크린샷과 페이지 읽기를 타고 컨텍스트로 들어오는 것을 막기 위한
// 것이다. "Realm 탭을 열지 않는다" 같은 규칙은 이미 렌더된 화면을 되돌리지 못하므로
// 시스템에서 막는다.
//
// 상대 경로(`./realmEndpoints`)와 alias 경로(`src/ts/realmEndpoints`)가 섞여 있어서
// 문자열 alias로는 전부 잡히지 않는다. 해석된 파일 경로를 보고 바꾼다.
const realmEndpointBlockPlugin: Plugin = {
  name: 'risunest-block-realm-endpoints',
  enforce: 'pre',
  resolveId: {
    // rolldown은 JS 훅을 부를 때마다 네이티브 경계를 넘는다. 필터를 주면 이름이
    // 후보인 import에 대해서만 이 훅을 부르므로 나머지 수만 건은 비용이 없다.
    // `.blocked.ts`는 필터에 걸리지 않아 되돌지 않는다.
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

// https://vitejs.dev/config/
export default defineConfig(({command, mode}) => {
  // `pnpm tauribuild:android` runs with `--mode android`, and `tauri android
  // build` additionally exports TAURI_ENV_PLATFORM=android. Either signal means
  // the bundle is about to be embedded into the Android cdylib, where source
  // maps are dead weight: they cannot be opened on device and they inflate the
  // APK by tens of megabytes. Desktop and web builds are untouched.
  const isAndroidBundle = mode === 'android' || process.env.TAURI_ENV_PLATFORM === 'android'
  // Android 판별은 TAURI_ENV_PLATFORM도 보므로, `--mode agent`로 Android를 빌드해도
  // 소스맵 제외 같은 기존 Android 처리가 그대로 유지된다.
  //
  // 단, vite는 `.env.<mode>`만 읽는다. `--mode agent`에서는 `.env.desktop`과
  // `.env.android`가 로드되지 않으므로, 그 두 파일이 가진 값은 `.env.agent`가 그대로
  // 들고 있어야 한다. 그 파일들을 바꾸면 `.env.agent`도 같이 바꾼다.
  const blockRealmEndpoints = mode === 'agent'
  return {
    plugins: [
      blockRealmEndpoints ? realmEndpointBlockPlugin : null,
      svelte({
        preprocess: vitePreprocess(),
        onwarn: (warning, handler) => {
          // disable a11y warnings
          if (warning.code.startsWith("a11y-")) return;
          handler(warning);
        },
      }),
      tailwindcss(),
      wasm(),
      command === 'build' ? strip({
        include: '**/*.(mjs|js|svelte|ts)'
      }) : null
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
    },
    // to make use of `TAURI_ENV_DEBUG` and other env variables
    // https://v2.tauri.app/reference/environment-variables/
    envPrefix: ["VITE_", "TAURI_"],
    build: {
      target:'baseline-widely-available',
      // don't minify for debug builds
      minify: process.env.TAURI_ENV_DEBUG === 'true' ? false : 'oxc',
      // Desktop release errors must remain translatable offline. Android excludes maps.
      sourcemap: !isAndroidBundle && (mode === 'desktop' || process.env.TAURI_ENV_PLATFORM === 'windows' || process.env.TAURI_ENV_DEBUG === 'true'),
      chunkSizeWarningLimit: 2000,
    },
    
    optimizeDeps:{
      exclude: [
        "@browsermt/bergamot-translator"
      ],
      needsInterop:[
        "@mlc-ai/web-tokenizers"
      ]
    },

    resolve:{
      alias:{
        'src':'/src',
      }
    },
    worker: {
      format: 'es'
    }
}
});
