"""Incremental binary frame decoder."""


class FrameDecoder:
    def __init__(self, max_size=1024):
        self.max_size = max_size

    def feed(self, chunk):
        frames = []
        while len(chunk) >= 4:
            length = int.from_bytes(chunk[:4], "big")
            if len(chunk) < length + 4:
                break
            frames.append(chunk[4:length + 4])
            chunk = chunk[length + 4:]
        return frames
