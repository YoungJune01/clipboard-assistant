# Clipboard Assistant

[![CI](https://github.com/YoungJune01/clipboard-assistant/actions/workflows/ci.yml/badge.svg)](https://github.com/YoungJune01/clipboard-assistant/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows-0078D4.svg)](#system-requirements)

Clipboard Assistant is a local-first clipboard manager for Windows. It is built
with Tauri 2, React 19, TypeScript, Rust, and SQLite.

The application keeps clipboard history on your computer, provides a compact
quick-paste panel, and includes optional backup and WebDAV sync features. WebDAV
sync is disabled by default and only connects to the server configured by the
user.

## Features

- Capture, search, favorite, categorize, and restore clipboard history
- Handle text, HTML, rich text, images, and copied file lists
- Open a compact quick-paste panel with configurable global shortcuts
- Recognize text and QR codes from local images using Windows APIs
- Configure retention, storage limits, excluded applications, and startup behavior
- Back up and restore the complete local clipboard library
- Optionally synchronize encrypted-in-transit backups with an HTTPS WebDAV server
- Store WebDAV credentials in Windows Credential Manager instead of the app database
- Use the interface in Simplified Chinese or English

## Privacy

Clipboard contents can contain sensitive information. Clipboard Assistant stores
its history locally in the Tauri application data directory. It does not include
telemetry or connect to a vendor-operated cloud service.

WebDAV synchronization is opt-in. When enabled, clipboard backups are sent to the
server selected by the user. HTTPS is recommended; using plain HTTP requires an
explicit confirmation in the application.

## System Requirements

Clipboard Assistant currently targets 64-bit Windows 10 and Windows 11.

Building from source requires:

- Node.js 22 or a current Node.js version supported by Vite 7
- Rust stable with the `x86_64-pc-windows-msvc` target
- Microsoft C++ Build Tools
- Microsoft Edge WebView2 Runtime

See the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for the
full Windows toolchain setup.

## Development

Install the frontend dependencies:

```powershell
npm ci
```

Start the application in development mode:

```powershell
npm run tauri dev
```

Build the frontend before running standalone Cargo checks because the Tauri
configuration expects the generated `dist` directory:

```powershell
npm run build
cargo check --manifest-path src-tauri/Cargo.toml --locked
```

## Tests

Run the frontend and Rust test suites:

```powershell
npm test
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

Run formatting and lint checks:

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings
```

A small number of Windows integration tests are ignored by default because they
modify the real system clipboard, register a global hotkey, or send keyboard
input. Review and run those tests manually in a disposable desktop session.

## Release Build

Create the optimized executable and Windows installers with:

```powershell
npm run tauri build
```

The generated artifacts are written below `src-tauri/target/release`. Local
builds are unsigned; distributing them without a code-signing certificate can
trigger a Microsoft Defender SmartScreen warning.

## Performance Tests

The Windows performance suite uses a deterministic SQLite database containing
10,000 mixed-format clipboard records. It exercises pagination, search, capture,
recognition queues, WebDAV failure cleanup, and a 24-hour-equivalent maintenance
loop without logging clipboard payloads.

```powershell
cargo test --manifest-path src-tauri/Cargo.toml tests::performance_windows -- --nocapture
```

## Contributing

Bug reports and focused pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md)
before making a change. Please report security issues privately as described in
[SECURITY.md](SECURITY.md).

## License

Clipboard Assistant is available under the [MIT License](LICENSE).
