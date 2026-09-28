class FrameDecoder:
    def __init__(self, max_size=1024):
        self.max_size = max_size
        self.pending = bytearray()

    def feed(self, chunk):
        self.pending.extend(chunk)
        frames = []
        while len(self.pending) >= 4:
            length = int.from_bytes(self.pending[:4], "big")
            if length > self.max_size:
                raise ValueError("frame too large")
            if len(self.pending) < length + 4:
                break
            frames.append(bytes(self.pending[4:length + 4]))
            del self.pending[:length + 4]
        return frames
