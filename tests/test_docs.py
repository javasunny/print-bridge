"""Validate repository Markdown structure and local link targets."""

import re
import unittest
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]


class DocumentationTests(unittest.TestCase):
    def test_markdown_structure(self):
        documents = list((ROOT / 'docs').rglob('*.md'))
        self.assertTrue(documents, 'No documentation found')
        for path in documents:
            with self.subTest(path=path.relative_to(ROOT)):
                text = path.read_text(encoding='utf-8')
                self.assertTrue(text.endswith('\n'))
                self.assertEqual(len(re.findall(r'^# ', text, re.M)), 1)
                fence = None
                for line in text.splitlines():
                    match = re.match(r'^(`{3,}|~{3,})', line)
                    if match:
                        marker = match[1]
                        if fence is None:
                            fence = marker
                        elif marker[0] == fence[0] and len(marker) >= len(fence):
                            fence = None
                self.assertIsNone(fence, 'Unclosed code fence')

    def test_numbered_sections(self):
        text = (ROOT / 'docs' / 'PRD.md').read_text(encoding='utf-8')
        sections = re.findall(r'^## (\d+)\. ', text, re.M)
        self.assertEqual([int(n) for n in sections], list(range(1, len(sections) + 1)))
        self.assertGreaterEqual(len(sections), 13)
        for parent, child in re.findall(r'^### (\d+)\.(\d+) ', text, re.M):
            self.assertIn(parent, sections)
            self.assertGreater(int(child), 0)

    def test_link_targets(self):
        for path in (ROOT / 'docs').rglob('*.md'):
            text = path.read_text(encoding='utf-8')
            for target in re.findall(r'\[[^\]]+\]\(([^)]+)\)', text):
                with self.subTest(path=path.name, target=target):
                    parsed = urlsplit(target)
                    if parsed.scheme:
                        self.assertIn(parsed.scheme, ('http', 'https'))
                        self.assertTrue(parsed.netloc)
                    elif parsed.path:
                        self.assertTrue((path.parent / unquote(parsed.path)).exists())


if __name__ == '__main__':
    unittest.main()
