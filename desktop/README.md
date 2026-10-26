# Print Bridge 桌面端

桌面管理通过 Tauri 本地命令连接 Rust 核心，不提供网页管理接口。界面为随应用打包的静态资源，无 npm 构建步骤。

## Windows 开发运行

安装 Rust MSVC 工具链、Visual Studio C++ 构建工具和 WebView2。先在仓库根目录准备随包打印程序及第三方材料，再启动：

```sh
python scripts/prepare_sumatra.py
cargo run --manifest-path desktop/Cargo.toml --locked
```

在“共享打印机”选择本机队列并保存；创建设备凭据后，设置 Windows 局域网监听地址，再启动服务。默认只监听 `127.0.0.1:8631`，不会自动对局域网开放。

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

Windows 安装包配置为 NSIS，包含整个 `resources` 目录，安装时按需下载 WebView2。使用 Tauri CLI 2 在本目录执行 `cargo tauri build --bundles nsis`。打包前必须运行资源准备脚本，并检查二进制、匹配源码及许可证齐全；本地 `cargo check` 不验证 Windows 安装包、签名或 WebView2 安装流程。

监听地址不自动创建防火墙规则。仅允许可信私有网络访问指定端口；当前 HTTP 连接不加密令牌和文档。macOS PDF 提交及认证行为须按 PRD 进行实机验证。
