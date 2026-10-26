# Print Bridge

Windows 多打印机 IPP 桥接 MVP，使用 Tauri 和 Rust。macOS 通过各打印机独立的 IPP 地址提交 PDF，Windows 端使用 SumatraPDF 调用指定打印队列。首次使用时在设置中手动下载打印组件。

当前为开发版本，已在 macOS 上交叉构建 Windows x64 EXE 和 NSIS 安装包。Windows 安装运行、macOS 系统打印对话框认证和真实出纸仍待实机验收。

## 已实现

- 本地桌面管理：共享打印机、手动能力配置、设备凭据、任务列表、服务启停及托盘。
- IPP 1.1/2.0 六项操作；打印机能力可匿名查询，提交及任务操作使用 HTTP Basic 设备令牌认证，设备只能查询、取消自己的任务。
- PDF 结构、加密、页数及大小检查；A4、1–99 份及能力允许的单双面。
- SQLite 与 PDF 持久化、全局串行调度、等待任务取消；重启后处理中任务进入待核查。
- 打印目标固定，不回退到默认打印机；超时或非零退出不会自动重打。
- 设置中手动下载 SumatraPDF 3.6.1 x64，固定官方地址与 SHA-256，校验成功后启用，下载失败可重试。
- 模拟与正式数据隔离，单实例文件锁，上传并发与任务容量限制，终态 PDF 超期清理。

## 运行

用户直接安装 Windows 安装包。启动后打开“设置 → 下载打印组件”，下载完成后配置共享打印机和设备凭据。

本机构建产物位于 `dist`：推荐安装 `Print Bridge_0.1.0_x64-setup.exe`，安装程序按需下载 WebView2；独立应用 `Print-Bridge-Windows-x64.exe` 要求系统已有 WebView2。两个文件均未签名。

从源码运行时，Windows 开发环境需要 Rust MSVC 工具链、Visual Studio C++ 构建工具和 WebView2：

```sh
cargo run --manifest-path desktop/Cargo.toml --locked
```

macOS 开发机可以运行模拟桌面，所有任务只记录模拟完成：

```sh
cargo run --manifest-path desktop/Cargo.toml --locked -- --simulate
```

先保存至少一台共享打印机，再创建设备凭据。设备标识作为用户名，令牌作为密码。令牌只展示一次，服务端仅存哈希；不要将凭据写进打印机 URL。

默认监听 `127.0.0.1:8631`。用于局域网时改为 Windows 的局域网 IP 和端口，并仅为私有网络配置 Windows 防火墙。当前 HTTP 不加密凭据与 PDF，只适用于可信局域网。

详细操作和打包见 [桌面说明](desktop/README.md)，真实设备验收见 [Windows 验证清单](docs/windows-validation.md)，需求见 [PRD](docs/PRD.md)。

## 自动化验证

在仓库根目录执行：

```sh
cargo test --locked
cargo fmt --check
node --test desktop/ui/model.test.mjs
node --check desktop/ui/app.js
cargo fmt --manifest-path desktop/Cargo.toml --check
cargo build --manifest-path desktop/Cargo.toml --locked
```

Rust 测试包括监听 `127.0.0.1` 随机端口的分块 HTTP/IPP 模拟测试；受限环境需允许本机端口绑定。所有自动测试均不执行真实打印。

## 当前边界

- macOS 本机模拟探测已能生成 PDF/A4 配置；原生添加、Basic 凭据保存和实际打印仍待 Windows/macOS 联机验证，不宣称完整 AirPrint 或 IPP Everywhere 兼容。
- 不支持 Create-Job/Send-Document、多文档或非 PDF 输入；未支持的打印选项会被拒绝。
- 暂无可靠的跨连接幂等标识，因此主动重复提交会创建独立任务，网络重试也可能重复。应用自身不会自动重试打印调用。
- 待核查任务保留文件并提示人工检查；当前没有人工归档操作。界面显示最近 1000 条任务。
- PDF 检查已有输入大小和页数上限，但还没有独立进程的解析超时与内存隔离。不要向开发版本提交不可信 PDF。
- 暂无 IPPS、自动发现、登录启动、自动升级或签名安装包。多打印机先全局串行执行。
- 能力由本地手动配置，应先在 Windows 驱动中验证。界面不提供彩色选择，颜色沿用驱动默认值。

终态 PDF 保留 24 小时；等待、处理中和待核查的文件不自动删除。数据放在 Tauri 用户应用数据目录的 `production` 或 `simulation` 子目录。

SumatraPDF 组件保存在同一应用数据根目录下的 `tools/sumatra`。安装包不包含该第三方可执行程序；仅在用户点击按钮后从官方网站下载。项目构建与测试均不依赖 Python。
