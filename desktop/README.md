# Print Bridge 桌面端

桌面管理通过 Tauri 本地命令连接 Rust 核心，不提供网页管理接口。界面为随应用打包的静态资源，无 npm 构建步骤。

## Windows 开发运行

安装 Rust MSVC 工具链、Visual Studio C++ 构建工具和 WebView2，然后启动：

```sh
cargo run --manifest-path desktop/Cargo.toml --locked
```

首次使用时在“设置”点击“下载打印组件”；从官方网站下载并校验 SumatraPDF 后即可使用。在“共享打印机”选择本机队列并保存；创建设备凭据后，设置 Windows 局域网监听地址，再启动服务。默认只监听 `127.0.0.1:8631`。

macOS 添加时，在“打印机与扫描仪 → 添加打印机 → IP”填写：Address 为 `Windows 局域网 IP:端口`，Protocol 为 IPP，Queue 为 `ipp/printers/桥接 ID`，Use 先选 Auto Select。更新后的服务允许匿名读取打印机 PDF/A4 能力，但打印任务仍需设备 ID 和令牌认证。若 Auto Select 仍选 Generic PostScript Printer，不要用该驱动提交任务；记录 macOS 版本与添加结果继续排查。

关闭窗口隐藏到系统托盘；在托盘选择“打开 Print Bridge”可恢复窗口。“停止服务并退出”先停止接收和领取任务，若仍有打印调用则拒绝退出并提示稍后重试，不强行终止打印。

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
