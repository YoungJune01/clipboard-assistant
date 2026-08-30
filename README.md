# 剪贴板助手（Clipboard Assistant）

[![CI](https://github.com/YoungJune01/clipboard-assistant/actions/workflows/ci.yml/badge.svg)](https://github.com/YoungJune01/clipboard-assistant/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows-0078D4.svg)](#系统要求)

简体中文 | [English](README_EN.md)

剪贴板助手是一款面向 Windows 的本地优先剪贴板管理工具，使用 Tauri 2、React 19、TypeScript、Rust 和 SQLite 构建。

它会在本机保存剪贴板历史，提供轻量的快速粘贴面板，并支持可选的本地备份和 WebDAV 同步。WebDAV 默认关闭，只有用户主动配置并启用后才会连接指定服务器。

## 界面预览

### 快速剪贴板

搜索和筛选剪贴记录，管理分组、收藏、置顶与备注，并通过快捷键快速粘贴。

<p align="center">
  <img src="docs/screenshots/quick-panel.png" alt="剪贴板助手快速面板" width="360">
</p>

### 设置

集中管理剪贴板监听、启动行为、外观、本地存储、WebDAV、图片识别和快捷键。

![剪贴板助手设置界面](docs/screenshots/settings.png)

> 截图使用脱敏的演示数据生成，不包含真实剪贴板内容、账号或本机文件路径。

## 主要功能

- 捕获、搜索、收藏、置顶、分组和恢复剪贴板历史
- 支持文本、HTML、富文本、图片和复制的文件列表
- 使用可配置的全局快捷键打开紧凑型快速粘贴面板
- 使用 Windows 本地能力识别图片文字和二维码
- 配置保存期限、空间上限、排除应用和开机启动行为
- 备份及恢复完整的本地剪贴板资料库
- 可选地通过 HTTPS WebDAV 同步备份
- WebDAV 凭据保存在 Windows 凭据管理器中，而非应用数据库
- 支持简体中文和英文界面

## 隐私说明

剪贴板可能包含敏感信息。剪贴板助手将历史记录保存在 Tauri 应用数据目录中，不包含遥测，也不会连接开发者运营的云服务。

WebDAV 同步为可选功能。启用后，应用会将剪贴板备份发送到用户指定的服务器。建议使用 HTTPS；使用普通 HTTP 时，应用会要求用户明确确认。

## 系统要求

目前支持 64 位 Windows 10 和 Windows 11。

从源码构建需要：

- Node.js 22，或 Vite 7 当前支持的 Node.js 版本
- Rust stable 和 `x86_64-pc-windows-msvc` target
- Microsoft C++ Build Tools
- Microsoft Edge WebView2 Runtime

完整工具链安装方式请参阅 [Tauri 前置要求](https://v2.tauri.app/start/prerequisites/)。

## 本地开发

安装前端依赖：

```powershell
npm ci
```

以开发模式启动桌面应用：

```powershell
npm run tauri dev
```

单独运行 Cargo 检查前，请先构建前端，因为 Tauri 配置需要生成的 `dist` 目录：

```powershell
npm run build
cargo check --manifest-path src-tauri/Cargo.toml --locked
```

## 测试

运行前端及 Rust 测试：

```powershell
npm test
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

运行格式和 lint 检查：

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings
```

少量 Windows 集成测试默认被忽略，因为它们会修改真实系统剪贴板、注册全局快捷键或发送键盘输入。请审查测试内容后，在可安全重置的桌面会话中手动运行。

## 构建安装包

生成优化后的可执行文件及 Windows 安装包：

```powershell
npm run tauri build
```

产物位于 `src-tauri/target/release`。本地构建未进行代码签名，直接分发时可能触发 Microsoft Defender SmartScreen 警告。

## 性能测试

Windows 性能测试会生成包含 10,000 条混合格式记录的确定性 SQLite 数据库，并覆盖分页、搜索、捕获、识别队列、WebDAV 失败清理和等效 24 小时维护循环，且不会记录剪贴板内容。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml tests::performance_windows -- --nocapture
```

## 参与贡献

欢迎提交问题报告和范围明确的 Pull Request。参与前请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)，安全问题请按照 [SECURITY.md](SECURITY.md) 私下报告。

## 开源许可

本项目基于 [MIT License](LICENSE) 开源。
