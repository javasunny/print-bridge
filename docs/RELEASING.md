# 发布指南

编译好的程序通过 [GitHub Releases](https://github.com/javasunny/print-bridge/releases) 分发。`dist` 和构建缓存不加入 Git；Release 必须对应一个已提交的源码版本和版本标签。

## 准备版本

1. 对齐 `desktop/Cargo.toml`、`desktop/Cargo.lock` 和 `desktop/tauri.conf.json` 的应用版本。
2. 更新 [更新日志](../CHANGELOG.md)、README 下载文件名和当前设计；不要把未验证的功能或设备标为支持。
3. 依赖变更后运行 `node scripts/generate-third-party-notices.mjs`，审查许可证、版权和源码入口。首次运行前需已经取得锁文件对应的 Cargo 依赖；该脚本只读取本地依赖，不会下载或安装组件。
4. 按 [README 测试步骤](../README.md#测试) 验证，确认待发布源码没有凭据、用户数据库或打印文档，再创建 Git commit。

如果上游 crate 归档缺失许可证，`third_party/license-overrides` 中保存精确上游提交的许可证补件及来源；升级这些依赖时也要更新补件。

## 构建

Windows 原生构建需要 Rust MSVC、Visual Studio C++ Build Tools、WebView2 与 Tauri CLI 2：

```sh
cd desktop
cargo tauri build --bundles nsis -- --locked
```

macOS 使用已准备的 cargo-xwin、LLVM/LLD、NSIS 与 Tauri CLI：

```sh
bash scripts/build-windows.sh
```

工具准备见 [桌面构建说明](../desktop/README.md#在-macos-构建-windows-x64)。交叉构建的安装包不能替代 Windows 实机验证。发布包目前未签名，不自动下载或打包 SumatraPDF。

## 发布文件

| 文件 | 说明 |
|---|---|
| `Print-Bridge_<version>_x64-setup.exe` | NSIS 安装包，从构建输出复制并将产品名中的空格替换为横线 |
| `Print-Bridge-Windows-x64.exe` | 独立程序，依赖已有 WebView2 |
| `LICENSE` | Print Bridge 的 MIT 协议 |
| `THIRD_PARTY_LICENSES.txt` | 第三方许可证、版权和精确源码下载入口 |
| `SumatraPDF-NOTICE.txt` | 用户另行下载组件的上游许可说明 |
| `enable-airprint-discovery.ps1` | 可选的私有网络 mDNS 防火墙修复脚本 |
| `SHA256SUMS.txt` | 上述文件的 SHA-256 校验值 |

安装包内也包含三个许可证文件。单独分发 EXE 时，应随附上述许可证材料，保留第三方声明。

## 创建 Release

### GitHub Actions 自动发布

仓库中的 `.github/workflows/release.yml` 在推送 `v*` 标签后自动执行 Windows x64 发布。普通代码推送不会触发发布；无需额外配置个人 Token，工作流使用 GitHub 提供的 `GITHUB_TOKEN` 和 `contents: write` 权限。

完成上述版本准备、相关测试及实机验收记录后，推送包含工作流的源码，再推送与应用版本完全一致的新标签。例如应用版本为 `0.2.2` 时：

```sh
git push origin HEAD
git tag v0.2.2
git push origin v0.2.2
```

工作流会核对标签、应用版本、Cargo 清单及对应更新日志，执行 Rust/前端/发布脚本测试、格式和依赖许可检查，使用 Windows 原生工具链构建 NSIS 安装包及独立 EXE，并收集上表中的全部文件。Release 说明从对应版本的更新日志生成，并附带安装方式和验证边界。

文件先上传到 Release 草稿，随后下载全部资产，核对文件清单和 SHA-256，通过后才公开发布。正式版本默认按正式版本发布；`-beta.N`、`-rc.N` 标签标记为 pre-release。已有同名 Release 不会覆盖；若上传或下载验证失败，会保留草稿供排查，修复后可删除尚未发布的草稿并在 Actions 页面重新运行。已经公开发布的程序变化必须使用新版本号。

在 GitHub 仓库的 Actions 页面查看执行进度。自动化构建不执行实体打印或 Apple 设备联机验收，相关结果仍应记录到 Windows 验证清单。

### 手动发布

推送已验证的源码 commit 后，用 `v<version>` 标签发布。Release 说明至少包含安装方式、首次下载打印组件、运行平台、主要变化、未签名提示及实际验证边界。默认按正式版本发布，不因版本处于 `0.x` 或存在已披露的验收限制而自动标记预发布。仅在用户明确要求试用版、测试版，或本次交付明确属于兼容性试验版本时，使用 `-beta`、`-rc` 等预发布版本号，并在 GitHub 标记为 pre-release。正式发布同样需要如实说明验证范围，不把未验收的设备或能力写成已支持。

发布前核对本地文件哈希与构建输出一致；发布后从 GitHub 核对标签指向的 commit、资产文件名、大小和上传后的 SHA-256，确认安装包和独立 EXE 均可下载。GitHub 自动提供源码 ZIP / tar.gz，不需要把本地缓存上传到 Release。
