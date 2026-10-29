# Print Bridge

让 Mac、iPhone 和 iPad 使用 Windows 电脑上已安装的打印机。

A local IPP print bridge for Windows, with experimental AirPrint discovery for Apple devices. Built with Rust and Tauri.

[![Release](https://img.shields.io/github/v/release/javasunny/print-bridge?include_prereleases)](https://github.com/javasunny/print-bridge/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![Windows x64](https://img.shields.io/badge/platform-Windows%20x64-blue.svg)](https://github.com/javasunny/print-bridge/releases)

> 当前为早期开发版本。已有 macOS → Windows 真实打印及自动发现恢复的用户验证记录；iPhone/iPad 实际出纸、设备认证和多队列等完整兼容性仍待实机验证。项目没有取得 Apple AirPrint 或 PWG IPP Everywhere 认证。

## 功能

- **共享多台打印机**：绑定 Windows 队列，为每台打印机提供独立的 IPP 地址和 Bonjour 广播。
- **Apple 设备接入**：实验性 AirPrint 自动发现；Mac 可通过 IPP 地址和队列专属 PPD 手动添加。
- **文档与打印选项**：支持 PDF、JPEG、300 dpi 灰度/sRGB URF 与 PWG Raster；按队列配置纸张、默认纸张、份数及双面能力。客户端显式彩色/黑白选项传递给 Windows 驱动。
- **本地任务管理**：SQLite 持久化、全局串行调度、等待任务取消；异常结果进入“待核查”，不自动重打。
- **设备凭据**：默认启用设备令牌认证，可在可信局域网中手动关闭；完整令牌只显示一次，服务端仅保存哈希。
- **记录与诊断**：按打印机筛选任务，查看连接来源及拒绝原因；支持清除已结束打印记录和诊断记录。
- **桌面与托盘**：本地管理界面、服务启停、SumatraPDF 手动下载和更新；关闭窗口后继续在托盘运行。

## 下载与安装

前往 [GitHub Releases](https://github.com/javasunny/print-bridge/releases) 下载编译好的文件，无需安装 Rust 或自行构建。

| 文件 | 用途 |
|---|---|
| `Print-Bridge_0.2.1_x64-setup.exe` | 推荐：Windows x64 安装包，按需安装 WebView2 |
| `Print-Bridge-Windows-x64.exe` | 独立程序，需要系统已有 Microsoft Edge WebView2 Runtime |
| `SHA256SUMS.txt` | 发布文件的 SHA-256 校验值 |
| `LICENSE`、`THIRD_PARTY_LICENSES.txt` | 项目及第三方依赖许可证，安装包也包含这些文件 |

运行目标为 **Windows 11 x64**，需提前安装打印机驱动，并确认 Windows 本机能够正常打印。其他 Windows 版本及 ARM64 尚未验收；macOS 仅支持开发模拟模式。

安装包尚未进行代码签名，Windows 可能显示 SmartScreen 或发布者提示。仅从本仓库 Release 下载，并可在 PowerShell 中核对摘要：

```powershell
Get-FileHash .\Print-Bridge_0.2.1_x64-setup.exe -Algorithm SHA256
```

首次下载打印组件和安装缺失的 WebView2 需要联网。完成准备后，打印通过局域网进行，不依赖云端打印服务。

## 快速开始

### 1. 配置 Windows

1. 安装并打开 Print Bridge，在“设置”中点击“下载组件”，等待 SumatraPDF 下载和校验完成。
2. 在“共享打印机”中填写桥接 ID、显示名称，选择 Windows 队列并保存。纸张、默认纸张和双面能力应与已验证的驱动能力一致。
3. 默认要求设备认证：在“设备凭据”创建凭据并保存设备 ID 与令牌。首次测试也可在“设置”关闭认证；运行中更改需重启服务。
4. 默认监听地址为 `0.0.0.0:8631`（全部 IPv4 网卡），可手动修改，点击“启动服务”。Mac 手动添加打印机时使用 Windows 的实际局域网 IPv4，不能填写 `0.0.0.0`。
5. Windows 网络应设为“专用”；在防火墙中仅对可信私有局域网放行服务 TCP 端口及 mDNS UDP `5353`。应用不会自动修改防火墙。发现异常时参阅 [AirPrint 排障与防火墙脚本](docs/airprint.md#只有重启服务才会被发现)。

### 2. 添加 Apple 设备

- **Mac**：打开“系统设置 → 打印机与扫描仪 → 添加”，在默认列表选择共享名称，Use 选择 AirPrint。
- **iPhone / iPad**：与 Windows 连接同一局域网，在应用的“打印 → 选择打印机”中选择共享名称。这些设备的完整联机兼容性仍待验收。
- **认证开启时**：用户名填设备 ID，密码填令牌。不要把凭据写入打印机 URL。

如果自动发现不可用，Mac 可以手动填写 IPP 地址，例如 `ipp://192.168.1.10:8631/ipp/printers/office`。填写具体监听 IP 时，界面提供独立的 Address、Protocol、Queue 和 PPD 下载地址；默认监听全部网卡时，请使用 Windows 的实际局域网 IP，详见 [桌面使用说明](desktop/README.md)。

### 3. 查看任务与诊断

“打印任务”和“连接诊断”在打开对应页时每 2 秒刷新。“已提交”只表示已交给 Windows，不代表确认出纸。

- 等待任务可以取消；待核查任务请检查 Windows 队列和实际出纸情况。
- 打印任务的“清除记录”删除**全部打印机**的已结束记录及对应缓存文档，不受当前筛选影响；等待、处理中和待核查任务保留。
- 诊断页保留本次运行最近 100 条请求；清除后继续记录新请求。任务界面显示最近 1000 条记录。
- 关闭窗口后服务继续在系统托盘运行；通过托盘菜单停止服务并退出。

## 工作原理

```text
Mac / iPhone / iPad
    │ IPP · PDF / JPEG / URF / PWG Raster
    ▼
Rust IPP 服务 → 格式校验与 PDF 转换 → SQLite + 文档存储
    │ 全局串行任务调度
    ▼
SumatraPDF 独立进程 → 指定 Windows 驱动与队列 → 打印机

Tauri 桌面界面 ── 本地命令 ── 配置、凭据、任务与诊断
```

实体打印机本身无需具备 AirPrint 能力。能力由本地手动配置；项目不会自动安装驱动，也不会把失败任务改投默认打印机。

## 安全与限制

- 仅用于**可信局域网**。当前使用 HTTP IPP 和 Basic 认证，不支持 IPPS；凭据和打印文档没有传输加密，不应暴露到公网。
- 关闭认证后，能连接服务的局域网客户端均可提交任务，且共用匿名任务身份。
- 不支持多文档任务、PostScript、任意栅格编码、纸盒或装订。没有登录启动、应用自动升级或待核查任务人工归档功能。
- 没有跨连接的可靠重试去重。主动重复提交及客户端网络重试可能创建独立任务；应用自身不会自动重试打印调用。
- 输入及转换后的 PDF 最多 100 MiB、500 页；有栅格解码、上传并发、任务数量和磁盘配额限制。PDF 解析尚无独立进程资源隔离，请勿接收不可信文档。
- 终态 PDF 保留 24 小时后清理，任务元数据不按天自动删除；等待、处理中和待核查文档保留。主动清除已结束记录会同时删除对应文档。
- 数据位于 Tauri 用户应用数据目录的 `production` / `simulation` 子目录；SumatraPDF 位于同一根目录的 `tools/sumatra`。应用升级不会主动清除这些数据。

## 从源码开发

Windows 需要 Rust MSVC 工具链、Visual Studio C++ Build Tools 和 WebView2。界面为原生 JavaScript 静态资源，无需 npm 安装或前端构建。

```sh
git clone https://github.com/javasunny/print-bridge.git
cd print-bridge
cargo run --manifest-path desktop/Cargo.toml --locked
```

macOS 仅用于模拟开发，不执行真实打印：

```sh
cargo run --manifest-path desktop/Cargo.toml --locked -- --simulate
```

在 Windows 使用 Tauri CLI 2 打包：

```sh
cd desktop
cargo tauri build --bundles nsis -- --locked
```

macOS 交叉构建 Windows 安装包使用 `bash scripts/build-windows.sh`。工具准备、产物说明和发布步骤见 [桌面构建说明](desktop/README.md#在-macos-构建-windows-x64) 与 [发布指南](docs/RELEASING.md)。

### 测试

Node.js 用于前端测试，Rust 测试不执行真实打印：

```sh
cargo test --locked
cargo fmt --check
node --test desktop/ui/*.test.mjs scripts/*.test.mjs
node --check desktop/ui/app.js
cargo fmt --manifest-path desktop/Cargo.toml --check
cargo check --manifest-path desktop/Cargo.toml --locked
```

部分测试需要绑定本机 TCP/UDP 端口。下载组件、创建系统临时打印队列和原生 Bonjour 测试默认忽略，须按说明单独运行；模拟测试不代表实体打印验收通过。

## 文档

- [桌面使用、组件与构建](desktop/README.md)
- [当前设计与实现边界](docs/PRD.md)
- [AirPrint 接入与排障](docs/airprint.md)
- [Windows 实机验证记录与清单](docs/windows-validation.md)
- [发布指南](docs/RELEASING.md)
- [更新日志](CHANGELOG.md)

## 反馈与贡献

欢迎提交 [Issue](https://github.com/javasunny/print-bridge/issues) 或 Pull Request。报告问题时请提供应用版本、Windows / Apple 系统版本、打印机与驱动、认证模式及已脱敏的诊断结果，不要上传令牌或打印文档。

提交代码前运行相关测试及格式检查。涉及打印行为的更改应补充回归测试；请明确区分模拟结果与真实设备验证，具体要求见 [贡献指南](CONTRIBUTING.md)。

## 开源协议与第三方软件

Print Bridge 自身代码使用 [MIT License](LICENSE)，允许使用、修改、分发和商用，需保留版权和许可证声明。第三方依赖沿用各自许可证，详见 [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt)。

SumatraPDF 为独立的第三方项目，**不随源码仓库或安装包分发**；用户在设置中主动从官方网站下载，应用通过独立进程调用。其许可证与源码入口见 [SumatraPDF 声明](third_party/SumatraPDF-NOTICE.txt)。
