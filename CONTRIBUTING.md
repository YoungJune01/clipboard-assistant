# Contributing

Thanks for taking the time to improve Clipboard Assistant.

## Before You Start

- Search existing issues before opening a new one.
- Keep reports free of real clipboard contents, credentials, database files, and
  other personal information.
- Open an issue before starting a large feature or architectural change.
- Keep pull requests focused on one problem.

## Development Setup

Clipboard Assistant is currently a Windows-only application. Install the
Windows prerequisites listed in the project README, then run:

```powershell
npm ci
npm run tauri dev
```

## Required Checks

Before submitting a pull request, run:

```powershell
npm run build
npm test
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

Some ignored Windows integration tests interact with the real clipboard,
global hotkeys, or keyboard input. Mention any relevant manual testing in the
pull request description.

## Pull Requests

- Describe the user-visible behavior and the reason for the change.
- Add or update tests when behavior changes.
- Do not commit generated `dist`, `target`, installer, database, backup, or
  credential files.
- Avoid unrelated formatting and refactoring.
- By contributing, you agree that your contribution is licensed under the MIT
  License used by this project.
