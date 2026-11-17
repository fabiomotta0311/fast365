"""Verifica a cobertura dos marcadores UNVERIFIED em código Rust: python docs/test_catalogo_evidencias.py."""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ID = re.compile(r'\b[A-Z]+(?:-[A-Z]+)+-\d{3}\b')


def marked_ids(path, text):
    found = set()
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if 'UNVERIFIED' not in line:
            continue
        ids = ID.findall(line)
        # Contexto permitido: início de item de comentário doc contíguo,
        # até três linhas acima; outros itens e linhas vazias são barreiras.
        continuation = re.match(r'^\s*(//!|///)\s{2,}(?!-\s)', line)
        if not ids and continuation:
            prefix = continuation.group(1)
            for previous in reversed(lines[max(0, index - 3):index]):
                match = re.match(r'^\s*' + re.escape(prefix) + r'(\s+)(\S.*)$', previous)
                if not match:
                    break
                content = match.group(2)
                if content.startswith('- '):
                    ids = ID.findall(content)
                    break
                if len(match.group(1)) < 2:
                    break
        if not ids:
            raise AssertionError(f'{path}:{index + 1}: marcador sem ID no próprio item ou linha')
        found.update(ids)
    return found


class CatalogTest(unittest.TestCase):
    def test_multiple_ids_and_generic_marker(self):
        self.assertEqual(marked_ids('x.rs', '//! HISTORY-PATH-001, HISTORY-SHAPE-001 e HISTORY-PAGE-001 (UNVERIFIED)'),
                         {'HISTORY-PATH-001', 'HISTORY-SHAPE-001', 'HISTORY-PAGE-001'})
        with self.assertRaisesRegex(AssertionError, 'marcador sem ID'):
            marked_ids('x.rs', '//! hipótese UNVERIFIED genérica')

    def test_nearby_id_must_belong_to_same_comment_bullet(self):
        self.assertEqual(marked_ids('arbitrary.rs', '//! - `EXAMPLE-CONTRACT-001` (PARCIAL): descrição\n//!   continuação\n//!   UNVERIFIED hipótese'), {'EXAMPLE-CONTRACT-001'})
        for text in (
            '//! - `EXAMPLE-CONTRACT-001` anterior\n//! - UNVERIFIED outra hipótese',
            '//! - `EXAMPLE-CONTRACT-001` anterior\n\n//! UNVERIFIED hipótese',
            'let x = "EXAMPLE-CONTRACT-001";\n// UNVERIFIED hipótese',
            '//! - `EXAMPLE-CONTRACT-001`\n//!   a\n//!   b\n//!   c\n//!   UNVERIFIED distante',
        ):
            with self.subTest(text=text), self.assertRaisesRegex(AssertionError, 'marcador sem ID'):
                marked_ids('arbitrary.rs', text)

    def test_all_rust_markers_have_catalog_rows(self):
        found = set()
        for file in (ROOT / 'crates').rglob('*.rs'):
            if 'target' in file.parts:
                continue
            found.update(marked_ids(file.relative_to(ROOT).as_posix(), file.read_text(encoding='utf-8')))
        rows = {}
        for line in (ROOT / 'docs/CATALOGO-EVIDENCIAS.md').read_text(encoding='utf-8').splitlines():
            match = re.match(r'^\|\s*`?(' + ID.pattern + r')`?\s*\|', line)
            if match:
                self.assertNotIn(match.group(1), rows, 'ID duplicado no catálogo')
                rows[match.group(1)] = line
        self.assertTrue(found, 'nenhum marcador encontrado')
        self.assertEqual(found - rows.keys(), set(), 'IDs UNVERIFIED ausentes no catálogo')
        for contract in found:
            self.assertIn('UNVERIFIED', rows[contract], contract)
        print(f'\nIDs UNVERIFIED no código: {len(found)}')


if __name__ == '__main__':
    unittest.main()
