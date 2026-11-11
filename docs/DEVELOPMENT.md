# 开发环境与启动指南

本文说明如何从零配置 Windows 开发环境，并完成 Octopus Island 的编译、启动、测试和打包。

## 1. 环境要求

| 项目 | 要求 |
| --- | --- |
| 操作系统 | Windows 10 / 11 x64 |
| Rust | `stable-x86_64-pc-windows-msvc` |
| C++ 工具链 | Visual Studio 2022 或 2026 Build Tools，勾选“使用 C++ 的桌面开发” |
| Windows SDK | Windows 11 SDK `10.0.26100.0` 或更新稳定版 |
| PowerShell | Windows PowerShell 5.1 或更高版本 |
| Git | 任意较新版本 |
| NSIS | 3.13，仅打包安装程序时需要 |

项目是 Rust + egui 的原生 Windows 程序，不需要 Node.js、Electron、Python 或 Java 运行时。

## 2. 安装开发工具

### 2.1 Git

下载地址：

- 官方页面：https://git-scm.com/download/win
- Windows x64 安装包：https://github.com/git-for-windows/git/releases/download/v2.56.1.windows.1/Git-2.56.1-64-bit.exe

安装后打开新的 PowerShell，确认：

```powershell
git --version
```

### 2.2 Visual Studio Build Tools

Build Tools 提供 MSVC 编译器和链接器，不需要安装完整的 Visual Studio IDE。

下载地址：

- 官方下载页：https://visualstudio.microsoft.com/downloads/
- 直接下载（VS 2022 Build Tools）：https://aka.ms/vs/17/release/vs_BuildTools.exe

安装步骤：

1. 以管理员身份运行 `vs_BuildTools.exe`
2. 在“工作负载”中勾选 **使用 C++ 的桌面开发**
3. 在“安装详细信息”中确认包含：
   - **MSVC 生成工具（最新，x64/x86）**
   - **Windows 11 SDK（10.0.26100.0 或更新版本）**
4. 安装完成后按提示重启电脑

验证：

```powershell
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
```

再确认 Windows SDK 的 `rc.exe` 存在：

```powershell
Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter rc.exe |
  Select-Object -ExpandProperty FullName
```

### 2.3 Rust

下载地址：

- Rustup 官方页面：https://rustup.rs/
- Windows x64 直接下载：https://win.rustup.rs/x86_64

安装步骤：

1. 运行 `rustup-init.exe`
2. 直接按回车，选择默认的 **Proceed with standard installation**
3. 安装完成后关闭并重新打开 PowerShell

设置 MSVC 工具链：

```powershell
rustup default stable-x86_64-pc-windows-msvc
```

验证：

```powershell
rustc -vV
cargo --version
rustup show
```

`rustc -vV` 的输出中必须包含：

```text
host: x86_64-pc-windows-msvc
```

#### 将 Rust 安装到 D 盘（可选）

默认会安装到 `%USERPROFILE%\.cargo` 和 `%USERPROFILE%\.rustup`。如果希望像 JDK 一样把工具链放到 D 盘，先设置用户环境变量，再运行 `rustup-init.exe`：

```powershell
[Environment]::SetEnvironmentVariable('CARGO_HOME', 'D:\Rust\cargo', 'User')
[Environment]::SetEnvironmentVariable('RUSTUP_HOME', 'D:\Rust\rustup', 'User')
```

关闭所有 PowerShell 后重新打开，确认：

```powershell
echo $env:CARGO_HOME
echo $env:RUSTUP_HOME
```

然后重新运行 `rustup-init.exe`，安装器会显示新的 Cargo 目录。

### 2.4 NSIS（可选，仅打包时需要）

下载地址：

- 官方下载页：https://nsis.sourceforge.io/Download
- NSIS 3.13 安装包：https://prdownloads.sourceforge.net/nsis/nsis-3.13-setup.exe?download

默认安装路径：

```text
C:\Program Files (x86)\NSIS\makensis.exe
```

也可以不安装 NSIS，直接使用项目脚本下载便携版：

```powershell
$compiler = .\scripts\get-nsis.ps1 -Directory $env:TEMP
```

## 3. 获取代码

```powershell
git clone https://github.com/CE-ND/Octopus-Island.git D:\Desktop\Octopus-Island
cd D:\Desktop\Octopus-Island
```

## 4. 编译和启动

### 4.1 开发模式运行

```powershell
cargo run --locked --bin octopus-island
```

这会编译调试版并启动程序，产物位于：

```text
target\debug\octopus-island.exe
```

建议开发时使用独立数据目录，避免覆盖日常设置：

```powershell
$env:OCTOPUS_ISLAND_DATA_DIR = Join-Path $env:TEMP 'octopus-island-development'
cargo run --locked --bin octopus-island
```

### 4.2 发布模式编译

```powershell
cargo build --locked --release --bin octopus-island
.\target\release\octopus-island.exe
```

发布版启用了优化、LTO 和静态 C 运行库，生成的是可直接分发的 `octopus-island.exe`。

### 4.3 只做编译检查

```powershell
cargo check --locked --all-targets --features record
```

第一次编译需要下载并编译全部依赖，通常需要数分钟；之后的增量编译会快很多。低内存设备可以设置：

```powershell
$env:CARGO_BUILD_JOBS = "1"
```

## 5. 运行测试

```powershell
cargo test --locked --all-targets --features record -- --test-threads=1
```

`--test-threads=1` 是必需的：部分测试通过进程环境变量隔离数据目录，单线程运行可以避免互相干扰。

带 `#[ignore]` 的测试需要真实账号、真实播放器或截图环境，按需显式运行，不要连接私人账号。

## 6. 打包 Windows 安装程序

需要先安装 NSIS 3.13，或通过 `-NsisCompiler` 指定 `makensis.exe`：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\package.ps1
```

或者使用便携版编译器：

```powershell
$compiler = .\scripts\get-nsis.ps1 -Directory $env:TEMP
.\scripts\package.ps1 -NsisCompiler $compiler
```

打包产物位于 `dist`：

```text
dist\Octopus-Island-Setup-v0.6.63-x64.exe
dist\Octopus-Island-v0.6.63-windows-x64.zip
dist\SHA256SUMS.txt
```

## 7. 项目结构

| 目录／文件 | 内容 |
| --- | --- |
| `src/main.rs` | Windows 外壳、系统状态和后台服务衔接 |
| `src/app.rs`、`src/pages.rs` | 状态与主界面 |
| `src/calendar.rs`、`src/calendar_ui.rs` | CalDAV、重复日程、月历与列表 |
| `src/mail.rs`、`src/mail_reader.rs` | IMAP、MIME、验证码与邮件阅读 |
| `src/quota*.rs` 及 provider 模块 | AI 额度与连接 |
| `src/native.rs`、`src/system.rs` | 原生窗口、输入、拖放和系统事件 |
| `src/ui_tests.rs` | 无窗口交互与布局回归测试 |
| `src/bin/octopus-island-record` | 合成数据演示与软件渲染器 |
| `vendor/` | 少量必要的上游补丁，见依赖说明 |
| `installer/`、`scripts/` | 安装、卸载与打包 |

## 8. 环境变量

| 变量 | 用途 |
| --- | --- |
| `OCTOPUS_ISLAND_DATA_DIR` | 指定开发实例的数据目录，避免覆盖 `%APPDATA%\OctopusIsland` |
| `OCTOPUS_ISLAND_RENDERER` | 设置为 `wgpu` 时强制使用 wgpu 图形后端 |
| `CARGO_HOME` | 自定义 Cargo 缓存和命令目录 |
| `RUSTUP_HOME` | 自定义 Rust 工具链目录 |

测试和截图相关的 `OCTOPUS_ISLAND_*` 变量见 `src/ui_tests.rs` 中的说明。

## 9. 常见问题

| 问题 | 处理方法 |
| --- | --- |
| `cargo` 无法识别 | 重新打开 PowerShell 或 VS Code；确认 `%USERPROFILE%\.cargo\bin` 或自定义的 `D:\Rust\cargo\bin` 已加入 PATH |
| `link.exe not found` | 确认 Visual Studio Build Tools 已勾选“使用 C++ 的桌面开发”和 MSVC 生成工具 |
| `rc.exe 未找到` | 安装 Windows 11 SDK，确认 `C:\Program Files (x86)\Windows Kits\10\bin` 下存在对应版本 |
| `toolchain 'stable-x86_64-pc-windows-msvc' is not installed` | 执行 `rustup toolchain install stable-x86_64-pc-windows-msvc` |
| 首次编译很慢 | 属于正常现象；Rust 依赖需要从源码编译，后续增量编译会明显加快 |
| crates.io 下载超时 | 检查网络或代理，也可以配置 Cargo 镜像 |
| `NSIS compiler not found` | 安装 NSIS 3.13，或给 `package.ps1` 传入 `-NsisCompiler` |
| 运行后看不到窗口 | 程序是桌面悬浮岛，检查屏幕顶部边缘和系统托盘 |
