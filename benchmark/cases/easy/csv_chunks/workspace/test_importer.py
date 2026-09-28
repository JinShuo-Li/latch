import unittest

from importer import parse_chunks


class ImportTest(unittest.TestCase):
    def test_plain_rows(self):
        self.assertEqual(parse_chunks(["a,b\nc,d\n"]), [["a", "b"], ["c", "d"]])

    def test_quoted_newline_and_chunk_boundary(self):
        self.assertEqual(parse_chunks(['name,note\n"Ada","one\n', 'two"\n']),
                         [["name", "note"], ["Ada", "one\ntwo"]])


if __name__ == "__main__":
    unittest.main()
