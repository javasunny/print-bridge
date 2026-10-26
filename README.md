# Print Bridge

Windows 多打印机 IPP 桥接 MVP，使用 Tauri 和 Rust。macOS 通过各打印机独立的 IPP 地址提交 PDF，Windows 端使用内置 SumatraPDF 调用指定打印队列。

当前为可编译的开发版本。协议与模拟队列可以在 macOS 验证，Windows 安装包、macOS 系统打印对话框认证和真实出纸仍待实机验收，不能据此认定完整打印链路已通过。

## 已实现

- 本地桌面管理：共享打印机、手动能力配置、设备凭据、任务列表、服务启停及托盘。
- IPP 1.1 六项操作，HTTP Basic 设备令牌认证；设备只能查询、取消自己的任务。
- PDF 结构、加密、页数及大小检查；A4、1–99 份及能力允许的单双面。
- SQLite 与 PDF 持久化、全局串行调度、等待任务取消；重启后处理中任务进入待核查。
- 打印目标固定，不回退到默认打印机；超时或非零退出不会自动重打。
- SumatraPDF 3.6.1 x64 资源准备脚本，固定 SHA-256，随包包含源码和许可证。
- 模拟与正式数据隔离，单实例文件锁，上传并发与任务容量限制，终态 PDF 超期清理。

## 运行

Windows 上先安装 Rust MSVC 工具链、Visual Studio C++ 构建工具、Python 3 和 WebView2，然后执行：

```sh
python scripts/prepare_sumatra.py
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
python3 -B -m unittest discover -s tests -v
node --test desktop/ui/model.test.mjs
node --check desktop/ui/app.js
cargo fmt --manifest-path desktop/Cargo.toml --check
cargo build --manifest-path desktop/Cargo.toml --locked
```

Rust 测试包括监听 `127.0.0.1` 随机端口的分块 HTTP/IPP 模拟测试；受限环境需允许本机端口绑定。所有自动测试均不执行真实打印。

## 当前边界

- 尚未承诺 macOS 原生添加方式能直接发送 PDF 和保存 Basic 凭据；不宣称 AirPrint 或 IPP Everywhere 兼容。
- 不支持 Create-Job/Send-Document、多文档或非 PDF 输入；未支持的打印选项会被拒绝。
- 暂无可靠的跨连接幂等标识，因此主动重复提交会创建独立任务，网络重试也可能重复。应用自身不会自动重试打印调用。
- 待核查任务保留文件并提示人工检查；当前没有人工归档操作。界面显示最近 1000 条任务。
- PDF 检查已有输入大小和页数上限，但还没有独立进程的解析超时与内存隔离。不要向开发版本提交不可信 PDF。
- 暂无 IPPS、自动发现、登录启动、自动升级或签名安装包。多打印机先全局串行执行。
- 能力由本地手动配置，应先在 Windows 驱动中验证。界面不提供彩色选择，颜色沿用驱动默认值。

终态 PDF 保留 24 小时；等待、处理中和待核查的文件不自动删除。数据放在 Tauri 用户应用数据目录的 `production` 或 `simulation` 子目录。
