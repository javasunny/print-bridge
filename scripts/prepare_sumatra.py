#!/usr/bin/env python3
"""Prepare pinned SumatraPDF binary, corresponding source and notices together."""

import argparse
import hashlib
import json
import shutil
import stat
import tempfile
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
MAX_ARCHIVE_BYTES = 150 * 1024 * 1024
MAX_MEMBER_BYTES = 200 * 1024 * 1024


def verify_archive(path, expected_sha256):
    digest = hashlib.sha256()
    size = 0
    with Path(path).open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            size += len(block)
            if size > MAX_ARCHIVE_BYTES:
                raise ValueError('Archive exceeds size limit')
            digest.update(block)
    if digest.hexdigest() != expected_sha256:
        raise ValueError('Archive SHA-256 does not match pinned release')


def safe_member(archive, name):
    entries = archive.infolist()
    if len({entry.filename for entry in entries}) != len(entries):
        raise ValueError('Duplicate archive paths')
    for entry in entries:
        path = PurePosixPath(entry.filename)
        if path.is_absolute() or '..' in path.parts or '\\' in entry.filename or ':' in entry.filename:
            raise ValueError('Unsafe archive path')
        if stat.S_ISLNK(entry.external_attr >> 16):
            raise ValueError('Archive symlink not allowed')
    entry = archive.getinfo(name)
    if entry.is_dir() or entry.file_size > MAX_MEMBER_BYTES:
        raise ValueError('Invalid archive member size or type')
    return archive.read(entry)


def acquire(spec, destination, local=None):
    if local:
        shutil.copyfile(local, destination)
    else:
        request = urllib.request.Request(spec['url'], headers={'User-Agent': 'print-bridge-build/0.1'})
        with urllib.request.urlopen(request, timeout=120) as response, destination.open('wb') as target:
            size = 0
            while block := response.read(1024 * 1024):
                size += len(block)
                if size > MAX_ARCHIVE_BYTES:
                    raise ValueError('Download exceeds size limit')
                target.write(block)
    verify_archive(destination, spec['sha256'])


def prepare(output, binary_archive=None, source_archive=None):
    lock = json.loads((ROOT / 'third_party/sumatra.lock.json').read_text())
    output = Path(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='sumatra-build-', dir=output.parent) as directory:
        stage = Path(directory)
        binary = stage / 'binary.zip'
        source = stage / 'SumatraPDF-source.zip'
        acquire(lock['binary'], binary, binary_archive)
        acquire(lock['source'], source, source_archive)
        with zipfile.ZipFile(binary) as archive:
            executable = safe_member(archive, lock['binary']['member'])
            if not executable.startswith(b'MZ'):
                raise ValueError('Expected a Windows executable')
            (stage / 'SumatraPDF.exe').write_bytes(executable)
        with zipfile.ZipFile(source) as archive:
            for name in ('COPYING', 'COPYING.BSD', 'AUTHORS'):
                (stage / name).write_bytes(safe_member(archive, lock['source']['prefix'] + name))
            # The complete source zip, including bundled dependencies and build
            # files, is retained; only extracting COPYING would be insufficient.
            for name in ('premake5.lua', 'mupdf/COPYING'):
                safe_member(archive, lock['source']['prefix'] + name)
        shutil.copyfile(ROOT / 'third_party/SumatraPDF-NOTICE.txt', stage / 'NOTICE.txt')
        (stage / 'release-manifest.json').write_text(json.dumps(lock, indent=2) + '\n')
        output.mkdir(parents=True, exist_ok=True)
        for name in ('SumatraPDF.exe', 'SumatraPDF-source.zip', 'COPYING', 'COPYING.BSD', 'AUTHORS', 'NOTICE.txt', 'release-manifest.json'):
            (stage / name).replace(output / name)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'desktop/resources/sumatra')
    parser.add_argument('--binary-archive', type=Path, help='Use a local archive; pinned SHA-256 still required')
    parser.add_argument('--source-archive', type=Path, help='Use a local source archive; pinned SHA-256 still required')
    args = parser.parse_args()
    result = prepare(args.output, args.binary_archive, args.source_archive)
    print(f'Prepared SumatraPDF 3.6.1 x64 with source and notices: {result}')


if __name__ == '__main__':
    main()
