# Clipboard Assistant

Clipboard Assistant is a Windows desktop application built with Tauri, React, and TypeScript.

## Development

Install dependencies and run the standard project checks:

```text
npm ci
npm run build
cargo check --manifest-path src-tauri/Cargo.toml --locked
```

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Performance Acceptance

The Windows performance suite uses a deterministic SQLite database containing 10,000 mixed-format clipboard records. It verifies 100 pages without duplicate IDs, bounded session payloads, repeated search, capture acknowledgement, recognition queue saturation and draining, WebDAV failure cleanup, and a 24-hour-equivalent maintenance loop. Timing output contains durations only and never clipboard payloads.

Run the suite with:

```text
cargo test --manifest-path src-tauri/Cargo.toml tests::performance_windows -- --nocapture
```

Acceptance targets and the August 29, 2026 local release measurements are:

| Measurement | Target | p50 | p95 |
| --- | ---: | ---: | ---: |
| Quick-panel controller show path | 300 ms | 1.4 us | 2.1 us |
| First 50-record page | 100 ms | 2.70 ms | 3.89 ms |
| Typical search over 10,000 records | 150 ms | 117.79 ms | 135.99 ms |
| Clipboard capture acknowledgement | 50 ms | 1.8 us | 4.7 us |

The quick-panel figure measures the Rust controller path through monitor placement, window show, and focus using deterministic window adapters. Windows hotkey registration and behavior remain covered by the Windows integration tests; external keyboard input through WebView rendering is not represented by this microbenchmark.

Idle memory was measured from the optimized x64 GUI executable after a 12-second stabilization period with the 10,000-record database loaded and OCR/WebDAV idle. Thirty samples at 500 ms intervals reported 6,766,592 bytes private memory at p50, p95, and maximum, plus 32,935,936 bytes working set. The release gate is less than 120 MiB private memory.
