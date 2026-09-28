import unittest

from framing import FrameDecoder


def frame(body):
    return len(body).to_bytes(4, "big") + body


class FramingTest(unittest.TestCase):
    def test_single(self):
        self.assertEqual(FrameDecoder().feed(frame(b"hello")), [b"hello"])

    def test_every_split(self):
        wire = frame(b"hello") + frame(b"world")
        for split in range(len(wire) + 1):
            decoder = FrameDecoder()
            self.assertEqual(decoder.feed(wire[:split]) + decoder.feed(wire[split:]),
                             [b"hello", b"world"])


if __name__ == "__main__": unittest.main()
