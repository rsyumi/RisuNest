## Project Overview

RisuNest is a cross-platform AI chatting application, a fork of RisuAI focused on large-library performance and native app targets. Built with:
- **Frontend**: Svelte 5 + TypeScript
- **Desktop/Mobile shell**: Tauri 2 (Rust backend; generated Android project in `src-tauri/gen/android`)
- **Build Tool**: Vite 8
- **Styling**: Tailwind CSS 4
- **Package Manager**: pnpm

The application allows users to chat with various AI models (OpenAI, Claude, Gemini, and more) through a single unified interface. It features a rich user interface with support for themes, plugins, custom assets, and advanced memory systems.

Platform priorities: Android and Windows first; macOS, iOS, and Linux second. All targets keep the Svelte WebView UI; a native Rust data core or Compose UI is adopted only when measurements justify it.

### Fork Scope

- Provider-specific request formatting, SSE decoders, and response parsers follow upstream RisuAI. Do not refactor them locally except for correctness fixes; merge-conflict cost outweighs the benefit.
- Optimization targets are the provider-independent paths: regex scripts, lorebook, Lua, stream postprocessing, rendering, storage, and sync.

## Directory Structure

```
RisuNest/
├── src/                    # Main application source code
│   ├── ts/                 # TypeScript business logic (tests colocated as *.test.ts)
│   ├── lib/                # Svelte UI components
│   ├── lang/               # Internationalization (i18n)
│   └── etc/                # Documentation and extras
├── src-tauri/              # Tauri backend (Rust); gen/android is the generated Android shell
├── server/                 # Self-hosting server implementations
│   ├── node/               # Node.js server (current)
│   └── hono/               # Hono framework server (future)
├── public/                 # Static assets
├── dist/                   # Build output
├── resources/              # Application resources
└── .github/workflows/      # CI/CD pipelines
```

### Source Code Structure (`/src`)

#### `/src/ts` - TypeScript Business Logic

| Directory/File | Purpose |
|----------------|---------|
| `storage/` | Persistence layer: revisioned persistent record store + active working set, save coordinator, BlobStore, sync adapters (`storage/sync/`), platform adapters |
| `process/` | Core processing logic (chat, requests, memory, models) |
| `plugins/` | Plugin system (API v3.0, sandboxing, security) |
| `gui/` | GUI utilities (colorscheme, highlight, animation) |
| `drive/` | Cloud sync and backup |
| `translator/` | Translation system |
| `model/` | Model definitions and integrations |
| `sync/` | Multi-user synchronization |
| `cbs.ts` | Callback system |
| `characterCards.ts` | Character card import/export |
| `parser.svelte.ts` | Message parsing |
| `stores.svelte.ts` | Svelte stores for state management |
| `globalApi.svelte.ts` | Global API methods |
| `bootstrap.ts` | Application initialization |

#### `/src/ts/process` - Core Processing

| Directory/File | Purpose |
|----------------|---------|
| `index.svelte.ts` | Main chat processing orchestration |
| `request/` | API request handlers (OpenAI, Anthropic, Google) |
| `memory/` | Memory systems (HypaMemoryV2/V3, SupaMemory, HanuraiMemory) |
| `models/` | AI model integrations (NAI, OpenRouter, Ooba, local models) |
| `templates/` | Prompt templates and formatting |
| `mcp/` | Model Context Protocol support |
| `files/` | File handling (inlays, multisend) |
| `embedding/` | Vector embeddings |
| `lorebook.svelte.ts` | Lorebook/world info management |
| `scriptings.ts` | Scripting system |
| `triggers.ts` | Event triggers |
| `stableDiff.ts` | Stable Diffusion integration |
| `tts.ts` | Text-to-speech |

#### `/src/lib` - Svelte UI Components

| Directory | Purpose |
|-----------|---------|
| `ChatScreens/` | Chat interface components |
| `UI/` | General UI components (GUI, NewGUI, Realm) |
| `Setting/` | Settings panels |
| `SideBars/` | Sidebar components (Scripts, LoreBook) |
| `Others/` | Miscellaneous components |
| `Mobile/` | Mobile-specific UI |
| `Playground/` | Testing/playground features |
| `VisualNovel/` | Visual novel mode |
| `LiteUI/` | Lightweight UI variant |

## Building and Running

### Prerequisites

- Node.js 20.19+ or 22.12+ and pnpm
- Rust and Cargo (for Tauri builds)
- Android SDK/NDK for Android builds (compile/target SDK 36, min SDK 24, NDK 28)

### Development

```bash
# Web development server
pnpm dev

# Tauri desktop development
pnpm tauri dev
```

### Production Builds

```bash
# Web build
pnpm build

# Web build for hosting
pnpm buildsite

# Tauri desktop build
pnpm tauribuild
pnpm tauri build

# Android debug APK (x86_64 emulator / arm64 device)
pnpm android:build:emulator
pnpm android:build:arm64

# Hono server build
pnpm hono:build
```

Set `VITE_DISABLE_REALM=true` for all automated tests, benchmarks, and agent-operated builds or app runs. It replaces RisuRealm network access with local synthetic data. The flag defaults to off; production builds leave it unset.

### Type Checking

```bash
pnpm check
```

## Development Conventions

### Coding Style

- The project uses Prettier for code formatting
- Ensure code is formatted before committing

### State Management

The project uses Svelte 5 Runes system:
- `$state`, `$derived`, `$effect` for reactive state
- Svelte stores (writable, readable) in `stores.svelte.ts`

Key stores:
- `DBState` - Database state
- `selectedCharID` - Current character
- `settingsOpen`, `sideBarStore`, `MobileGUI` - UI state
- `loadedStore`, `alertStore` - Application state
- `DynamicGUI` - Responsive layout switching

### Styling & Theming

To ensure dynamic theme support across the app, always use the project's custom theme colors defined in `src/styles.css` when styling components with Tailwind CSS. If you need to check how these colors are dynamically managed or view available presets (like dark, light, cherry, etc.), reference `src/ts/gui/colorscheme.ts`. Only inspect this file when specifically working on theme-related logic.

Available custom theme colors include:
- `textcolor`, `textcolor2`
- `bgcolor`, `darkbg`, `darkbutton`, `selected`
- `borderc`, `darkborderc`
- `draculared`

You can safely apply Tailwind's opacity modifiers directly to these custom theme colors (e.g., `text-textcolor/90`, `bg-textcolor/5`, `border-textcolor/10`).

### File Naming Conventions

- `.svelte.ts` - Svelte 5 files with runes
- `.svelte` - Svelte component files
- Use camelCase for file names

### Testing

- `pnpm test` runs the Vitest suite (colocated `*.test.ts` files, hundreds of tests)
- `pnpm check` for type checking
- `pnpm benchmark:phase1` runs the deterministic regex benchmark with fixed output hashes
- Android JVM tests live under `src-tauri/gen/android` (Gradle)

## Key Architectural Patterns

### Data Layer

- The persistent runtime (revisioned IndexedDB record store, `src/ts/storage/persistentDataStore.ts`) is the authoritative store; `DBState` remains the in-memory compatibility working copy for existing UI and plugin paths
- Blob/asset storage behind the BlobStore interface with multiple backends (Tauri FS, LocalForage, OPFS, Node)
- Sync capabilities are separated adapters in `src/ts/storage/sync/`: official account snapshot, Drive snapshot, and manifest delta (the delta adapter has no server yet; do not extend it)
- `RisuSave` (`.bin`, with encryption support) is the import/export/migration format, not the live store; lossless migration uses staged import with validation before activation
- Character cards: Import/export in various formats (.risum, .risup, .charx)

### Processing Pipeline

1. Chat processing in `process/index.svelte.ts`
2. Request handling with provider abstraction
3. Memory systems for context management
4. Lorebook integration for world info

### Plugin System (API v3.0)

- Iframe-based sandboxing for security
- SafeDocument/SafeElement wrappers for DOM access
- Plugin storage (save-specific and device-specific)
- Custom AI provider support
- Hot reload support for development
- Two data-access profiles: scalable API v3 queries, and maximum-compatibility mode preserving the API v2.1 live Proxy

See `plugins.md` for comprehensive plugin development guide.

### UI Architecture

- Component-based with Svelte 5
- Responsive design with mobile/desktop variants
- Theme system with custom color schemes
- Multiple UI modes: Classic, WaifuLike, WaifuCut
- Dynamic GUI switching based on viewport
- No traditional router; uses conditional rendering in App.svelte
- In-app drag-and-drop uses custom MIME types to avoid conflicting with file imports; see `src/ts/dragTypes.ts`

## Supported AI Providers

- OpenAI (GPT series)
- Anthropic (Claude)
- Google (Gemini)
- DeepInfra
- OpenRouter
- AI Horde
- Ollama
- Ooba (Text Generation WebUI)
- Custom providers via plugins

## Internationalization

Supported languages:
- English (en)
- Korean (ko)
- Chinese Simplified (cn)
- Chinese Traditional (zh-Hant)
- Vietnamese (vi)
- German (de)
- Spanish (es)

Language files are located in `/src/lang/`.

## Deployment Targets

- **Android (Tauri)**: primary target; debug APK builds (arm64, x86_64) verified, release/store packaging pending
- **Desktop (Tauri)**: Windows (NSIS) primary; macOS (DMG, APP) and Linux (DEB, RPM, AppImage) secondary
- **iOS**: planned secondary target, not yet set up
- **Web**: Vite static site
- **Docker**: Container (port 6001)
- **Self-hosted**: Node.js or Hono server

## Security

- Plugin sandboxing with iframe isolation
- DOM sanitization with DOMPurify
- Buffer encryption/decryption utilities
- CORS handling with proxy support
- Tauri HTTP plugin for native fetch

## Documentation

| File | Description |
|------|-------------|
| `README.md` | Main project documentation |
| `plugins.md` | Plugin development guide |
| `AGENTS.md` | AI assistant documentation |
| `src/ts/plugins/migrationGuide.md` | Plugin API migration guide |
| `server/hono/README.md` | Hono server documentation |
| `server/node/readme.md` | Node server documentation |

## Contribution Guidelines

1. Follow the existing coding style and conventions
2. Run `pnpm check` before submitting a pull request
3. Ensure your code is well-tested
4. Format code with Prettier before committing
