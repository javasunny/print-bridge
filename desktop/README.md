# Print Bridge 桌面端

桌面管理通过 Tauri 本地命令连接 Rust 核心，不提供网页管理接口。界面为随应用打包的静态资源，无 npm 构建步骤。

## Windows 开发运行

安装 Rust MSVC 工具链、Visual Studio C++ 构建工具和 WebView2，然后启动：

```sh
cargo run --manifest-path desktop/Cargo.toml --locked
```

首次使用时在“设置”点击“下载打印组件”；从官方网站下载并校验 SumatraPDF 后即可使用。在“共享打印机”选择本机队列并保存；需要认证时创建设备凭据，再启动服务。启动界面自动检测本机局域网 IPv4 并填入 `IP:8631`；可手动修改，检测失败时不会回退到回环地址。

免认证测试：在“设置”取消勾选“要求设备认证”后自动保存。运行中修改会提醒重启服务，当前服务继续使用原认证策略。Mac 无需设备 ID 或令牌；所有免认证任务共用匿名身份。恢复认证同样勾选后重启服务。撤销设备会立即删除其凭据，不保留已撤销条目，历史任务仍保留。

每台共享打印机分别显示并提供复制按钮：Address、Protocol、Queue。macOS 添加时，在“打印机与扫描仪 → 添加打印机 → IP”填写：Address 为 `Windows 局域网 IP:端口`，Protocol 为 IPP，Queue 为 `ipp/printers/桥接 ID`。若 Auto Select 仍选 Generic PostScript Printer，在 Use 选择 Other，打开仓库内的 [Print-Bridge-PDF-A4-Simplex.ppd](../macos/Print-Bridge-PDF-A4-Simplex.ppd)，确认显示 `Print Bridge PDF A4 Single-Sided` 后添加。此配置用于 A4 单面队列，不提供双面选项；不含地址或凭据。认证开启时，打印任务使用设备 ID 和令牌。

关闭窗口隐藏到系统托盘；在托盘选择“打开 Print Bridge”可恢复窗口。“停止服务并退出”先停止接收和领取任务，若仍有打印调用则拒绝退出并提示稍后重试，不强行终止打印。

## 打印组件更新

在“设置”点击“检查更新”，读取官方稳定版版本号；停止打印服务且当前调用完成后，可下载组件或更新。3.6.1 继续使用锁定的 ZIP/EXE SHA-256；其他稳定版在 Windows 验证有效 Authenticode 签名、预期发布者和产品版本，之后记录本地摘要供离线校验。下载或校验失败会保留旧组件，不自动执行打印。

## 连接诊断

Windows 启动服务后打开“连接诊断”，可看到最近 100 条 HTTP/IPP 请求的来源 IP、时间、队列路径、操作、HTTP/IPP 状态和拒绝原因。连接诊断和打印任务使用横向表格，打开对应页时每 2 秒自动刷新。诊断记录只保留在本次运行的内存中，不记录令牌、请求头、文档标题或 PDF 内容。

在 Mac 的仓库根目录执行以下只读探测，替换 Windows IP、端口和桥接 ID：

```sh
ipptool -tv ipp://192.168.8.159:8631/ipp/printers/changhong-printer macos/check-ipp-connection.test
```

探测显示 PASS，且 Windows 出现对应的 Get-Printer-Attributes 记录，说明 Mac 能访问该服务。添加打印机或探测成功不代表已经提交打印任务；提交后应出现 Print-Job。若被拒绝，诊断页显示错误原因；若始终只有查询记录，则尚未收到打印文件。只读探测不会创建任务或真实打印。

0.1.3 起，不支持的任务属性会显示具体属性名。只有实际超过大小上限才返回 HTTP 413；连接中断或请求体读取失败返回 HTTP 400，并显示传输错误说明。PDF 上限仍为 100 MiB。Mac 的私有作业票据作为客户端元数据处理，标准份数、纸张、单双面和其他输出选项仍须通过验证。

## 模拟验证

macOS 只支持桌面模拟验证，不能作为真实打印服务：

```sh
cargo run --manifest-path desktop/Cargo.toml --locked -- --simulate
```

模拟模式提供两台模拟队列，并由核心使用独立的 `simulation` 数据目录。正常模式使用 `production` 目录；根目录为 Tauri 用户应用数据目录。模拟完成不代表 Windows 驱动或实际出纸通过验收。

## 检查与打包

```sh
node --check desktop/ui/app.js
node --test desktop/ui/model.test.mjs
cargo fmt --manifest-path desktop/Cargo.toml --check
cargo check --manifest-path desktop/Cargo.toml --locked
```

Windows 安装包配置为 NSIS，安装时按需下载 WebView2。使用 Tauri CLI 2 在本目录执行 `cargo tauri build --bundles nsis`。SumatraPDF 由用户在设置中另行下载；安装包不含其 EXE 或源码归档。构建不需要 Python。

## 在 macOS 构建 Windows x64

按 [Tauri 官方交叉构建说明](https://v2.tauri.app/distribute/windows-installer/) 安装 LLVM、LLD、NSIS，使用 rustup 提供的 Rust 工具链和 Windows MSVC 标准库：

```sh
brew install llvm lld nsis
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin --version 0.23.1 --locked
cargo install tauri-cli --version 2.12.4 --locked
bash scripts/build-windows.sh
```

以上命令在仓库根目录执行。脚本支持标准工具安装，也可复用本项目 `.build-tools` 内隔离安装的工具。首次构建会下载 Windows SDK 并缓存，后续可复用。

输出到根目录的 `dist`：独立应用 `Print-Bridge-Windows-x64.exe` 和 NSIS `*-setup.exe`。建议在 Windows 11 x64 上使用安装包，它会按需安装 WebView2；直接运行独立 EXE 要求系统已有 WebView2。输出尚未签名，交叉构建成功不代表 Windows 实机打印验收通过。

监听地址不自动创建防火墙规则。仅允许可信私有网络访问指定端口；当前 HTTP 连接不加密令牌和文档。macOS PDF 提交及认证行为须按 PRD 进行实机验证。
