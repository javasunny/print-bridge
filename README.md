# Print Bridge

Windows 多打印机 IPP 桥接 MVP，使用 Tauri 和 Rust。macOS 通过各打印机独立的 IPP 地址提交 PDF，Windows 端使用 SumatraPDF 调用指定打印队列。首次使用时在设置中手动下载打印组件。

当前为开发版本，已在 macOS 上交叉构建 Windows x64 EXE 和 NSIS 安装包。用户已验证 macOS 通过 Windows 首次真实打印成功；设备认证和多队列等场景仍需继续实机验收。

## 已实现

- 本地桌面管理：共享打印机、手动能力配置、设备凭据、任务列表、服务启停及托盘。
- IPP 1.1/2.0 六项操作；打印机能力可匿名查询，默认使用 HTTP Basic 设备令牌认证。可在设置中关闭认证，先测试免认证打印流程。
- PDF 结构、加密、页数及大小检查；A3、A4、A5、A6、Letter、Legal、Tabloid，1–99 份及能力允许的单面、长边双面、短边双面。
- SQLite 与 PDF 持久化、全局串行调度、等待任务取消；重启后处理中任务进入待核查。
- 打印目标固定，不回退到默认打印机；超时或非零退出不会自动重打。
- 设置中检查 SumatraPDF 官方稳定版并手动下载或更新；锁定版核对 SHA-256，新版在 Windows 校验发布者签名和版本，失败保留旧组件。
- 模拟与正式数据隔离，单实例文件锁，上传并发与任务容量限制，终态 PDF 超期清理。
- 每台打印机独立配置已验证的纸张、默认纸张和双面能力，并提供匹配配置的 macOS PPD 下载地址。旧配置与历史任务保持 A4 默认值。
- 连接诊断：显示来源 IP、请求时间、IPP 操作和处理结果，包含尚未进入任务列表的查询与拒绝请求。

## 运行

用户直接安装 Windows 安装包。启动后打开“设置 → 下载打印组件”，下载完成后配置共享打印机和设备凭据。

本机构建产物位于 `dist`：推荐安装 `Print Bridge_0.1.5_x64-setup.exe`，安装程序按需下载 WebView2；独立应用 `Print-Bridge-Windows-x64.exe` 要求系统已有 WebView2。两个文件均未签名。

从源码运行时，Windows 开发环境需要 Rust MSVC 工具链、Visual Studio C++ 构建工具和 WebView2：

```sh
cargo run --manifest-path desktop/Cargo.toml --locked
```

macOS 开发机可以运行模拟桌面，所有任务只记录模拟完成：

```sh
cargo run --manifest-path desktop/Cargo.toml --locked -- --simulate
```

先保存至少一台共享打印机，再创建设备凭据。设备标识作为用户名，令牌作为密码。令牌只展示一次，服务端仅存哈希；不要将凭据写进打印机 URL。

需要先跑通免认证流程时，在“设置”取消勾选“要求设备认证”，更改立即保存；若服务正在运行，界面会提醒重启服务后生效。该设置在应用重启后保留；关闭期间连接到服务的局域网设备均可打印，匿名任务共用一个身份。恢复认证同样勾选后重启服务。撤销凭据会删除该凭据，历史打印任务仍保留。

“打印任务”和“连接诊断”采用横向表格，在打开对应页时每 2 秒自动刷新；诊断保留最近 100 条请求，关闭程序后清空。Mac 可使用 [只读 IPP 探测](macos/check-ipp-connection.test)验证连通性，操作说明见 [桌面说明](desktop/README.md#连接诊断)。探测不会创建打印任务；只有 Print-Job 成功才表示 PDF 已进入桥接任务列表。

启动界面自动填入本机局域网 IPv4 和端口 `8631`，可手动修改；检测失败时提示手动填写。共享打印机按 macOS 的 Address、Protocol、Queue 分开显示并支持复制。仅为私有网络配置 Windows 防火墙。当前 HTTP 不加密凭据与 PDF，只适用于可信局域网。

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

- 用户已通过 macOS 原生队列验证首次免认证 PDF 打印；Basic 凭据保存和更多驱动场景仍待联机验证，不宣称完整 AirPrint 或 IPP Everywhere 兼容。
- 不支持 Create-Job/Send-Document、多文档或非 PDF 输入；未支持的打印选项会被拒绝。
- 暂无可靠的跨连接幂等标识，因此主动重复提交会创建独立任务，网络重试也可能重复。应用自身不会自动重试打印调用。
- 待核查任务保留文件并提示人工检查；当前没有人工归档操作。界面显示最近 1000 条任务。
- PDF 检查已有输入大小和页数上限，但还没有独立进程的解析超时与内存隔离。不要向开发版本提交不可信 PDF。
- 暂无 IPPS、自动发现、登录启动、自动升级或签名安装包。多打印机先全局串行执行。
- 能力由本地手动配置，应先在 Windows 驱动中验证。界面不提供彩色选择，颜色沿用驱动默认值。

终态 PDF 保留 24 小时；等待、处理中和待核查的文件不自动删除。数据放在 Tauri 用户应用数据目录的 `production` 或 `simulation` 子目录。

SumatraPDF 组件保存在同一应用数据根目录下的 `tools/sumatra`。安装包不包含该第三方可执行程序；仅在用户点击按钮后从官方网站下载。项目构建与测试均不依赖 Python。
