# App icon sources

- `logo.svg`: the RisuNest mark on its rounded gradient square (510x510 viewBox). Source for every desktop, iOS, web, and legacy Android icon.
- `bg.svg`, `fg.svg`, `mono.svg`: Android adaptive icon layers (full-bleed gradient background, foreground mark placed inside the 66dp safe zone, monochrome silhouette for themed icons).
- `icon-manifest.json`: `tauri icon` manifest wiring the files above together.

Regenerate every platform icon from the repository root:

```powershell
pnpm tauri icon src-tauri/icon-src/icon-manifest.json
```

This rewrites `src-tauri/icons/*` (Windows, macOS, Linux, iOS) and writes the Android launcher icons directly into `src-tauri/gen/android/app/src/main/res/` because that directory exists.

Web favicons and PWA icons in `public/` are plain PNG renders of `logo.svg`:

```powershell
pnpm tauri icon src-tauri/icon-src/logo.svg -p 16,32,192,256,512,1024 -o tmp-icons
```

Copy the results to `public/logo_16.png`, `logo_32.png`, `logo_192.png`, `logo_256.png`, `logo_512.png`, and `logo2.png` (the 1024 px one), then delete `tmp-icons`.

The wordmark (`public/wordmark.svg`, `public/wordmark-transparent.svg`) uses this mark plus "RisuNest" set in Montserrat ExtraBold, converted to outlines (SIL Open Font License); no font is needed at runtime.
