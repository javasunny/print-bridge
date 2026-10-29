import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { copyFileSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const sha256 = (file) => createHash('sha256').update(readFileSync(file)).digest('hex');

export function releaseInfo(directory, tag) {
  const version = JSON.parse(readFileSync(join(directory, 'desktop/tauri.conf.json'), 'utf8')).version;
  assert.match(version, /^\d+\.\d+\.\d+(?:-(?:beta|rc)\.\d+)?$/);
  assert.equal(tag, `v${version}`, 'Tag must match the application version');
  const manifest = readFileSync(join(directory, 'desktop/Cargo.toml'), 'utf8');
  const pkg = manifest.split('[package]')[1].split(/\n\[/)[0];
  assert.ok(pkg.split(/\r?\n/).includes(`version = "${version}"`), 'Cargo.toml version mismatch');
  const lock = readFileSync(join(directory, 'desktop/Cargo.lock'), 'utf8').replaceAll('\r\n', '\n');
  assert.ok(lock.includes(`name = "print-bridge-desktop"\nversion = "${version}"`), 'Cargo.lock version mismatch');
  const changelog = readFileSync(join(directory, 'CHANGELOG.md'), 'utf8');
  const section = changelog.split(/^## /m).find((part) => part.startsWith(`${version} —`));
  assert.ok(section, 'Missing version section in CHANGELOG.md');
  const changes = section.slice(section.indexOf('\n') + 1).trim();
  assert.ok(changes, 'Release changelog must not be empty');
  return { version, changes };
}

export function packageRelease(directory, version) {
  const output = join(directory, 'dist');
  mkdirSync(output); // Refuse stale files from an earlier build.
  const release = join(directory, 'desktop/target/x86_64-pc-windows-msvc/release');
  const installer = `Print Bridge_${version}_x64-setup.exe`;
  const files = [
    [join(release, 'bundle/nsis', installer), installer.replaceAll(' ', '-')],
    [join(release, 'print-bridge-desktop.exe'), 'Print-Bridge-Windows-x64.exe'],
    [join(directory, 'LICENSE'), 'LICENSE'],
    [join(directory, 'THIRD_PARTY_LICENSES.txt'), 'THIRD_PARTY_LICENSES.txt'],
    [join(directory, 'third_party/SumatraPDF-NOTICE.txt'), 'SumatraPDF-NOTICE.txt'],
    [join(directory, 'scripts/enable-airprint-discovery.ps1'), 'enable-airprint-discovery.ps1'],
  ];
  for (const [source, name] of files) copyFileSync(source, join(output, name));
  writeFileSync(join(output, 'SHA256SUMS.txt'), files.map(([, name]) => `${sha256(join(output, name))}  ${name}\n`).join(''));
}

export function verifyRelease(directory, expectedDirectory) {
  const expected = readFileSync(join(expectedDirectory, 'SHA256SUMS.txt'), 'utf8');
  assert.equal(readFileSync(join(directory, 'SHA256SUMS.txt'), 'utf8'), expected, 'Downloaded checksum file differs');
  const names = ['SHA256SUMS.txt'];
  for (const line of expected.trim().split('\n')) {
    const [, hash, name] = /^(\w{64})  (.+)$/.exec(line);
    assert.equal(sha256(join(directory, name)), hash, `Checksum mismatch: ${name}`);
    names.push(name);
  }
  assert.deepEqual(readdirSync(directory).sort(), names.sort(), 'Unexpected or missing release assets');
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [command, destination] = process.argv.slice(2);
  if (command === 'prepare') {
    const { version, changes } = releaseInfo(root, process.env.GITHUB_REF_NAME);
    writeFileSync(destination, `# Print Bridge ${version}\n\n${changes}\n\n` +
      '## 安装与使用\n\n' +
      `- Windows x64：下载安装 \`Print-Bridge_${version}_x64-setup.exe\`，按需安装 WebView2。\n` +
      '- 独立 EXE 需要已有 WebView2，并应同时保留随附的许可证文件。\n' +
      '- 首次使用请在设置中下载打印组件；SumatraPDF 不随安装包分发。\n\n' +
      '## 已知限制与验证范围\n\n' +
      '- 程序未签名，Windows 可能显示安全提示。\n' +
      '- GitHub Actions 验证自动化测试、格式、依赖许可和 Windows x64 原生打包；' +
      '发布文件已下载并校验 SHA-256。\n' +
      '- 自动构建不代表实体打印、驱动或 Apple 设备联机验收通过；' +
      '请参阅仓库 docs/windows-validation.md 中的实机验证记录。\n\n' +
      `源码 commit：${process.env.GITHUB_SHA}\n`);
  } else if (command === 'package') {
    packageRelease(root, releaseInfo(root, process.env.GITHUB_REF_NAME).version);
  } else if (command === 'verify') {
    verifyRelease(destination, join(root, 'dist'));
  } else {
    throw new Error(`Unknown release command: ${command}`);
  }
}
