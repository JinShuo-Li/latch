#!/usr/bin/env python3
"""Render actual deterministic Ratatui text output; requires optional Pillow."""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

root = Path(__file__).resolve().parent.parent
source = root / 'crates/latch-tui/tests/snapshots/v4_wide_sidebar.txt'
text = source.read_text()
(root / 'site/captures/terminal.txt').write_text(text)
font_path = '/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf'
font = ImageFont.truetype(font_path, 11)
image = Image.new('RGB', (1440, 650), '#181b20')
draw = ImageDraw.Draw(image)
for index, line in enumerate(text.splitlines()):
    draw.text((30, 22 + index * 15), line, font=font, fill='#d7dae0')
image.save(root / 'site/assets/terminal.png', optimize=True)
print('Rendered actual 200 × 40 Ratatui fixture to site/assets/terminal.png')
