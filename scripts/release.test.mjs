import assert from 'node:assert/strict';
import { cpSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { packageRelease, releaseInfo, verifyRelease } from './release.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const version = JSON.parse(readFileSync(join(root, 'desktop/tauri.conf.json'), 'utf8')).version;

function fixture(t) {
  const directory = mkdtempSync(join(tmpdir(), 'print-bridge-release-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  for (const name of [
    'desktop/tauri.conf.json', 'desktop/Cargo.toml', 'desktop/Cargo.lock',
    'CHANGELOG.md', 'LICENSE', 'THIRD_PARTY_LICENSES.txt',
    'third_party/SumatraPDF-NOTICE.txt', 'scripts/enable-airprint-discovery.ps1',
  ]) {
    mkdirSync(dirname(join(directory, name)), { recursive: true });
    cpSync(join(root, name), join(directory, name));
  }
  const release = join(directory, 'desktop/target/x86_64-pc-windows-msvc/release');
  mkdirSync(join(release, 'bundle/nsis'), { recursive: true });
  writeFileSync(join(release, 'bundle/nsis', `Print Bridge_${version}_x64-setup.exe`), 'fixture installer');
  writeFileSync(join(release, 'print-bridge-desktop.exe'), 'fixture executable');
  return directory;
}

test('release version must match the tag, manifests, and a nonempty changelog', (t) => {
  const directory = fixture(t);
  assert.equal(releaseInfo(directory, `v${version}`).version, version);
  assert.throws(() => releaseInfo(directory, 'v99.99.99'), /Tag must match/);
  writeFileSync(join(directory, 'desktop/Cargo.lock'), '');
  assert.throws(() => releaseInfo(directory, `v${version}`), /Cargo.lock version mismatch/);
});

test('missing or empty version notes prevent release', (t) => {
  const directory = fixture(t);
  writeFileSync(join(directory, 'CHANGELOG.md'), '# Changelog\n\n## Unreleased\n\n- Future change\n');
  assert.throws(() => releaseInfo(directory, `v${version}`), /Missing version section/);
  writeFileSync(join(directory, 'CHANGELOG.md'), `# Changelog\n\n## ${version} — 2026-10-29\n\n`);
  assert.throws(() => releaseInfo(directory, `v${version}`), /must not be empty/);
});

test('packaging includes installer, executable, notices, script, and verifiable hashes', (t) => {
  const directory = fixture(t);
  packageRelease(directory, version);
  const downloaded = join(directory, 'downloaded');
  cpSync(join(directory, 'dist'), downloaded, { recursive: true });
  verifyRelease(downloaded, join(directory, 'dist'));
  assert.equal(readFileSync(join(downloaded, `Print-Bridge_${version}_x64-setup.exe`), 'utf8'), 'fixture installer');
  assert.throws(() => packageRelease(directory, version), /EEXIST/);
});

test('tampered binaries, missing assets, and changed checksums prevent publication', (t) => {
  const directory = fixture(t);
  packageRelease(directory, version);
  const output = join(directory, 'dist');
  const downloaded = join(directory, 'downloaded');
  cpSync(output, downloaded, { recursive: true });
  const executable = join(downloaded, 'Print-Bridge-Windows-x64.exe');
  writeFileSync(executable, 'corrupted download');
  assert.throws(() => verifyRelease(downloaded, output), /Checksum mismatch/);
  rmSync(executable);
  assert.throws(() => verifyRelease(downloaded, output), /ENOENT/);
  writeFileSync(join(downloaded, 'SHA256SUMS.txt'), 'altered checksums');
  assert.throws(() => verifyRelease(downloaded, output), /Downloaded checksum file differs/);
});

test('a missing installer cannot produce a release', (t) => {
  const directory = fixture(t);
  rmSync(join(directory, 'desktop/target/x86_64-pc-windows-msvc/release/bundle/nsis'), { recursive: true });
  assert.throws(() => packageRelease(directory, version), /ENOENT/);
});
