import hashlib
import importlib.util
import io
import json
import stat
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('prepare_sumatra', Path(__file__).resolve().parents[1] / 'scripts/prepare_sumatra.py')
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SumatraResourceTests(unittest.TestCase):
    def test_rejects_wrong_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'archive.zip'
            path.write_bytes(b'fixture')
            MODULE.verify_archive(path, hashlib.sha256(b'fixture').hexdigest())
            with self.assertRaises(ValueError):
                MODULE.verify_archive(path, '0' * 64)

    def test_rejects_path_traversal_before_reading(self):
        for unsafe in ('../escape.exe', '/absolute.exe', 'C:/escape.exe', 'dir\\escape.exe'):
            with self.subTest(path=unsafe):
                buffer = io.BytesIO()
                with zipfile.ZipFile(buffer, 'w') as archive:
                    archive.writestr('expected.exe', b'MZfixture')
                    archive.writestr(unsafe, b'danger')
                with zipfile.ZipFile(buffer) as archive:
                    with self.assertRaises(ValueError):
                        MODULE.safe_member(archive, 'expected.exe')

    def test_only_reads_requested_member(self):
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, 'w') as archive:
            archive.writestr('expected.exe', b'MZfixture')
        with zipfile.ZipFile(buffer) as archive:
            self.assertEqual(MODULE.safe_member(archive, 'expected.exe'), b'MZfixture')
            with self.assertRaises(KeyError):
                MODULE.safe_member(archive, 'other.exe')

    def test_rejects_symlinks(self):
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, 'w') as archive:
            entry = zipfile.ZipInfo('expected.exe')
            entry.create_system = 3
            entry.external_attr = (stat.S_IFLNK | 0o777) << 16
            archive.writestr(entry, '/outside')
        with zipfile.ZipFile(buffer) as archive:
            with self.assertRaises(ValueError):
                MODULE.safe_member(archive, 'expected.exe')

    def test_preparation_keeps_source_and_notices_with_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            third_party = root / 'third_party'
            third_party.mkdir()
            binary = root / 'binary.zip'
            source = root / 'source.zip'
            with zipfile.ZipFile(binary, 'w') as archive:
                archive.writestr('Sumatra.exe', b'MZfixture')
            with zipfile.ZipFile(source, 'w') as archive:
                for name in ('COPYING', 'COPYING.BSD', 'AUTHORS', 'premake5.lua', 'mupdf/COPYING'):
                    archive.writestr('source/' + name, name.encode())
            manifest = {
                'binary': {'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'member': 'Sumatra.exe'},
                'source': {'sha256': hashlib.sha256(source.read_bytes()).hexdigest(), 'prefix': 'source/'},
            }
            (third_party / 'sumatra.lock.json').write_text(json.dumps(manifest))
            (third_party / 'SumatraPDF-NOTICE.txt').write_text('fixture notice')
            output = root / 'resources'
            with patch.object(MODULE, 'ROOT', root):
                MODULE.prepare(output, binary, source)
            self.assertEqual((output / 'SumatraPDF.exe').read_bytes(), b'MZfixture')
            self.assertEqual((output / 'SumatraPDF-source.zip').read_bytes(), source.read_bytes())
            self.assertEqual((output / 'NOTICE.txt').read_text(), 'fixture notice')
            self.assertEqual(json.loads((output / 'release-manifest.json').read_text()), manifest)
            for name in ('COPYING', 'COPYING.BSD', 'AUTHORS'):
                self.assertEqual((output / name).read_text(), name)


if __name__ == '__main__':
    unittest.main()
