#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tools_dir="$project_dir/.build-tools"

if [[ -x "$tools_dir/cargo/bin/rustup" ]]; then
  export RUSTUP_HOME="$tools_dir/rustup"
  export PATH="$tools_dir/cargo/bin:$PATH"
fi
export PATH="$tools_dir/xwin-bin:$tools_dir/portable-bin:/opt/homebrew/opt/llvm/bin:/opt/homebrew/opt/llvm@22/bin:/opt/homebrew/opt/lld/bin:$PATH"
export XWIN_CACHE_DIR="${XWIN_CACHE_DIR:-$tools_dir/xwin}"
export XWIN_ARCH=x86_64

if ! command -v cargo-xwin >/dev/null; then
  echo "缺少 cargo-xwin。请按 desktop/README.md 安装交叉构建工具。" >&2
  exit 1
fi
if ! command -v lld-link >/dev/null || ! command -v llvm-rc >/dev/null || ! command -v makensis >/dev/null; then
  echo "缺少 Windows 链接或打包工具：需要 lld-link、llvm-rc、makensis。" >&2
  exit 1
fi

cd "$project_dir/desktop"
if [[ -x "$tools_dir/tauri/node_modules/.bin/tauri" ]]; then
  "$tools_dir/tauri/node_modules/.bin/tauri" build --runner cargo-xwin --target x86_64-pc-windows-msvc -- --locked
else
  cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc -- --locked
fi

release_dir="$project_dir/desktop/target/x86_64-pc-windows-msvc/release"
mkdir -p "$project_dir/dist"
cp "$release_dir/print-bridge-desktop.exe" "$project_dir/dist/Print-Bridge-Windows-x64.exe"
find "$release_dir/bundle/nsis" -maxdepth 1 -type f -name '*-setup.exe' -exec cp {} "$project_dir/dist/" \;
echo "Windows 文件已生成到 $project_dir/dist"
