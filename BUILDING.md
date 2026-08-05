# Building Lumen

Lumen's desktop client is **Tauri 2 + a native Rust voice core**. The Rust side
pulls in `webrtc-audio-processing` (WebRTC APM, bundled C++ via meson/ninja) and
`audiopus_sys` (libopus via CMake), so a full C++/build toolchain is required on
every OS. There are two supported build paths: **GitHub Actions** (free on this
public repo) and **local** (the commands below).

## GitHub Actions (recommended — produces all three OS installers)

The repo is **public**, so Actions minutes are free. `release.yml` builds the
installers for Linux/macOS/Windows and publishes a **draft release** with only
the installers (`.deb/.rpm/.AppImage`, `.msi`, `.dmg`).

### Trigger by tag (recommended)

```bash
git tag v0.2.3
git push origin v0.2.3   # fires .github/workflows/release.yml
```

### Trigger manually (no tag)

```bash
gh workflow run release.yml -f version=0.2.3
```

### Get / publish the draft

```bash
# list the draft release
gh release list --exclude-drafts --repo BrizhelDev/lumen

# open in the browser
gh release view v0.2.3 --web

# publish it
gh release edit v0.2.3 --draft=false --repo BrizhelDev/lumen
```

> Note: an outstanding account balance on the GitHub account holds *all* Actions
> runs ("recent account payments have failed"). Clear it in
> **Settings → Billing & plans** before triggering.

## Local build — Windows

Prerequisites (install once):

- [Rust (MSVC toolchain)](https://rustup.rs)
- Node.js ≥ 22 and [pnpm ≥ 10](https://pnpm.io)
- Visual Studio 2022 Build Tools with the C++ workload
- Python 3 (for meson/ninja) and [LLVM](https://releases.llvm.org/) (for
  bindgen/libclang)
- CMake (for `audiopus_sys`); ensure it is on `PATH`
- WebView2 runtime (preinstalled on Windows 10/11)

Build:

```powershell
git clone https://github.com/BrizhelDev/lumen.git
cd lumen/apps/desktop
pnpm install
python -m pip install meson ninja   # bundled WebRTC APM build
pnpm build                          # frontend (Svelte)
pnpm tauri build                    # → src-tauri\target\release\bundle\msi\*.msi (+ NSIS .exe)
```

The `.msi` lands in `src-tauri\target\release\bundle\msi\`.

## Local build — Linux / macOS

Linux needs the Tauri system deps plus ALSA/meson/ninja/libclang:

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf \
  libasound2-dev pkg-config cmake build-essential meson ninja-build libclang-dev
```

macOS: `brew install cmake pkg-config meson ninja`

Then, from `apps/desktop`:

```bash
pnpm install
pnpm build
pnpm tauri build
```

## Gotchas

- `tauri build` reads the `CI` environment variable into its `--ci` flag. If
  your shell has `CI=1`, the CLI errors with `invalid value '1' for '--ci'`.
  Run it as `env -u CI pnpm tauri build`.
- The vendored WebKitGTK patch (`vendor/wry`) is committed and wired via
  `[patch.crates-io]` in `apps/desktop/src-tauri/Cargo.toml`; it is required to
  build and is already in the repo.
