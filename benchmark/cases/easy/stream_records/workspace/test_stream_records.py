import unittest

from stream_records import parse_records


class StreamRecordsTest(unittest.TestCase):
    def test_multiple_records(self):
        self.assertEqual(
            parse_records([b'{"id":1}\n{"id":2}\n']), [{"id": 1}, {"id": 2}]
        )

    def test_line_split_between_reads(self):
        self.assertEqual(
            parse_records([b'{"id":', b'3}\n']), [{"id": 3}]
        )

    def test_utf8_character_split_between_reads(self):
        self.assertEqual(
            parse_records([b'{"name":"\xe4', b'\xb8\xad"}\n']),
            [{"name": "中"}],
        )


if __name__ == "__main__":
    unittest.main()
