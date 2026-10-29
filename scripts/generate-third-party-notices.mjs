import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const metadata = JSON.parse(execFileSync('cargo', [
  'metadata', '--manifest-path', join(root, 'desktop/Cargo.toml'),
  '--format-version', '1', '--locked', '--offline', '--filter-platform', 'x86_64-pc-windows-msvc',
], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 }));
const overrides = join(root, 'third_party/license-overrides');
const sources = JSON.parse(readFileSync(join(overrides, 'sources.json'), 'utf8'));
const texts = new Map();
const catalog = [];

function licenseFiles(directory, depth = 0) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory() && depth < 2) return licenseFiles(path, depth + 1);
    return entry.isFile() && /^(licen[cs]e|copying|copyright|notice)/i.test(entry.name) ? [path] : [];
  }).sort();
}

for (const pkg of metadata.packages.filter((pkg) => pkg.source).sort((a, b) => `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`, 'en'))) {
  const directory = dirname(pkg.manifest_path);
  const files = licenseFiles(directory);
  if (pkg.license_file && !files.includes(join(directory, pkg.license_file))) files.push(join(directory, pkg.license_file));
  const extra = Object.keys(sources).filter((name) => name.startsWith(`${pkg.name}-${pkg.version}-`));
  files.push(...extra.map((name) => join(overrides, name)));
  if (!pkg.license || !files.length) throw new Error(`Missing license material: ${pkg.name} ${pkg.version}`);
  const references = files.map((file) => {
    const content = readFileSync(file, 'utf8').replace(/\r\n/g, '\n').trim();
    const digest = createHash('sha256').update(content).digest('hex');
    if (!texts.has(digest)) texts.set(digest, { id: texts.size + 1, content });
    const source = file.startsWith(overrides)
      ? sources[relative(overrides, file).split(sep).join('/')]
      : relative(directory, file).split(sep).join('/');
    return `  License text ${texts.get(digest).id}: ${source}`;
  });
  catalog.push([
    `${pkg.name} ${pkg.version}`, `  License: ${pkg.license}`,
    `  Source: https://crates.io/api/v1/crates/${pkg.name}/${pkg.version}/download`,
    ...(pkg.repository ? [`  Repository: ${pkg.repository}`] : []), ...references,
  ].join('\n'));
}

const output = [
  'Print Bridge third-party licenses and notices',
  '',
  'Generated from desktop/Cargo.lock by scripts/generate-third-party-notices.mjs.',
  'This inventory conservatively includes build/development and other-platform',
  'dependencies as well as Windows runtime dependencies. It does not relicense them.',
  'Unmodified MPL-2.0 covered dependencies are available in Source Code Form under',
  'MPL-2.0 at the exact crate Source download links below. Upstream license and',
  'copyright notices are preserved in those archives and the texts below.',
  'SumatraPDF is downloaded separately by the user and is not bundled.',
  '', 'DEPENDENCIES', '', catalog.join('\n\n'), '', 'LICENSE TEXTS', '',
  ...[...texts.values()].map(({ id, content }) => `=== License text ${id} ===\n\n${content}\n`),
].join('\n') + '\n';
const target = join(root, 'THIRD_PARTY_LICENSES.txt');
if (process.argv.includes('--check')) {
  if (readFileSync(target, 'utf8') !== output) throw new Error('THIRD_PARTY_LICENSES.txt is out of date');
} else writeFileSync(target, output);
console.log(`Third-party notices: ${catalog.length} packages, ${texts.size} distinct license texts.`);
