# AirPrint 与无驱动打印

Print Bridge 提供实验性的 AirPrint / IPP Everywhere 接入能力。Apple 设备连接桥接服务，Windows 端仍使用已安装的实体打印机驱动和 SumatraPDF；实体打印机本身无需支持 AirPrint。没有取得 Apple AirPrint 或 PWG IPP Everywhere 认证，自动测试不能替代 Windows、iPhone/iPad 与实体打印机联机验收。

## 使用

1. 在 Windows 安装 0.1.6 或更新版本，下载打印组件并保存共享队列的真实纸张和双面能力。
2. 使用可供局域网访问的监听地址启动服务。Windows 防火墙仅在私有网络放行监听 TCP 端口（默认 8631）与 mDNS UDP 5353。程序不自动修改防火墙。
3. Mac 在“打印机与扫描仪 → 添加”的默认列表选择共享名称，Use 选择 AirPrint，不需要下载或手动选择 PPD。已安装的旧 PPD 队列不会自动转换，需要重新添加。
4. iPhone/iPad 与 Windows 在同一局域网，在应用的“打印 → 选择打印机”中选择共享名称。
5. 认证开启时，用户名使用设备 ID，密码使用令牌。首次免认证联机测试可在设置中关闭认证并重启服务；程序不会自动关闭认证。认证模式在真实 Apple 设备上仍需验证。

共享名称带有短标识，避免多队列名称冲突。服务每分钟重新公告全部共享队列，帮助客户端刷新发现记录；停用队列或停止服务时停止刷新并撤销广播，更新纸张/双面配置时更新广播。Mac 可能缓存旧能力，修改后可重新添加队列。

自动发现状态显示在桌面服务区域。广播失败不会中断手动 IPP 打印。不同 VLAN、访客网络或无线客户端隔离可能阻止发现；Mac 可继续使用各队列的 Address、Protocol、Queue，旧 PDF PPD 仍可作为备用。回环地址监听不会发布局域网打印机。

## 只有重启服务才会被发现

2026-10-26 联机验证：打印 TCP 8631 可达，但 Bonjour 基础 IPP 和 AirPrint 子类型查询均无应答。用户在 Windows 管理员 PowerShell 执行下述脚本后确认界面自动发现恢复正常，支持本次故障由发现流量的入站防火墙配置引起的判断。现有 0.1.6 无需更新安装包。本机 `ippfind` 后续复测仍未返回队列，未将命令行联机检查记为通过；恢复结果以用户界面反馈为依据。

TCP 8631 可连接，不代表 Bonjour 的入站 UDP 5353 已放行。服务重启时主动发出的公告可以被 Apple 设备收到；若随后客户端查询被 Windows 防火墙阻止，就会出现“发现界面打开后，必须重启 Windows 服务才出现”的现象。先检查 Windows 当前网络属于“专用”，再放行发现查询；不要通过关闭整个防火墙或周期性重启服务处理。

保持 Windows Print Bridge 服务运行，将 [enable-airprint-discovery.ps1](../scripts/enable-airprint-discovery.ps1) 复制到 Windows，在**管理员 PowerShell** 中进入脚本目录并执行：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\enable-airprint-discovery.ps1
```

自定义监听端口时追加 `-ListenPort 你的端口`。脚本从该监听端口查找正在运行的 Print Bridge 可执行文件，只为该程序建立“Private + LocalSubnet + 入站 UDP 5353”规则；重复运行会更新同一规则，不改变网络类别，也不开放 Public 或 Domain 网络。脚本需要管理员权限，因为 Windows 防火墙规则修改受系统权限控制。安装包和应用仍不会自动修改防火墙。

执行后保持服务运行，在 Mac/iPhone/iPad 重新打开打印机选择界面验证，通常无需重启服务。若仍无法发现，需继续检查显式阻止规则、安全软件或网络对组播的限制。可用以下命令撤销脚本创建的规则：

```powershell
Remove-NetFirewallRule -Name PrintBridge-AirPrint-mDNS
```

## 协议与格式

- 为每个启用队列发布 `_ipp._tcp.local.` 与 AirPrint `_universal._sub._ipp._tcp.local.`，TXT 中包含实际端口/队列、认证方式、文档格式、URF 与 UUID。
- 支持 Print-Job，以及 Create-Job → Send-Document 的单文档提交。尚未收到文档或结束上传的任务不会启动打印；可取消，超时后终止，不自动重放。
- PDF 直接沿用原校验；JPEG、URF 与 PWG Raster 先转换为 PDF，再沿用持久化、调度和 Windows 打印后端。
- 栅格声明收敛到 300 dpi、8 位灰度与 24 位 sRGB。支持范围以服务返回的 `urf-supported`、`pwg-raster-document-type-supported` 和分辨率属性为准；不支持的编码拒绝，不把任意字节当 PDF 打印。
- 开放已配置纸张、份数及单双面；客户端请求的 auto/color/monochrome 随任务保存，显式彩色或黑白传递到 Windows 驱动，auto 沿用驱动默认。实际彩色出纸取决于实体打印机能力。没有提供 IPPS，当前凭据和文档使用明文 HTTP，只适用于可信局域网。
- 输入与转换后的 PDF 各最多 100 MiB、最多 500 页；单页解码像素最多 98 MiB，全任务累计解码像素最多 512 MiB，逐页压缩存储。渐进式 JPEG 最多 8M 像素。栅格文件压缩率与 PDF 不同，过大的任务可能被拒绝。

## 验证

常规运行 `cargo test --locked`，包含 IPP 属性、任务状态、文档转换及 macOS CUPS 栅格兼容测试；不会真实打印。

在 macOS 上显式运行无手工 PPD 队列测试（需要本机打印机管理权限）：

```sh
cargo test --locked --test macos_ipp native_driverless_queue -- --ignored --nocapture
cargo test --locked native_bonjour -- --ignored --nocapture
```

第一项测试创建独立的临时 CUPS 队列，使用 `lpadmin -m everywhere` 读取能力，提交到回环模拟服务，最后取消残留任务并删除该队列。第二项测试临时发布 Bonjour 服务，用系统 ippfind 验证发现与注销。两项测试都不操作实体打印机。

联机验收应检查：Mac 与 iPhone/iPad 可发现每个队列；不用 PPD 可以选择正确纸张；JPEG/URF/PWG Raster 能打印；份数和两种双面方向正确；认证、取消、停止和重启符合预期。任务“已提交”只表示交给 Windows，仍不代表确认出纸。

参考：[PWG IPP Everywhere](https://www.pwg.org/ipp/everywhere.html)、[PWG IPP 指南](https://www.pwg.org/ipp/ippguide.html)、[Apple AirPrint](https://support.apple.com/en-ie/102895)。
