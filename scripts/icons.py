"""Generate original, dependency-free tomato icons for app and template tray."""
import math
import pathlib
import struct
import zlib

OUT = pathlib.Path(__file__).resolve().parents[1] / "src-tauri" / "icons"
OUT.mkdir(exist_ok=True)

def png(size, tray=False):
    pixels = bytearray()
    for y in range(size):
        pixels.append(0)
        for x in range(size):
            nx, ny = (x + .5) / size, (y + .5) / size
            body = ((nx - .5) / .35) ** 2 + ((ny - .57) / .32) ** 2 < 1
            leaf = .13 < ny < .35 and abs(nx - .5) < .23 * (1 - abs(ny - .24) / .11)
            stem = .47 < nx < .53 and .1 < ny < .32
            if tray:
                color = (0, 0, 0, 255) if body or leaf or stem else (0, 0, 0, 0)
            else:
                color = (64, 139, 86, 255) if leaf or stem else (229, 86, 72, 255) if body else (0, 0, 0, 0)
            pixels.extend(color)
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(pixels)) + chunk(b'IEND', b'')

(OUT / 'icon.png').write_bytes(png(512))
(OUT / 'tray.png').write_bytes(png(32, True))
payload = b''
for kind, size in [(b'icp4', 16), (b'icp5', 32), (b'icp6', 64), (b'ic07', 128), (b'ic08', 256), (b'ic09', 512), (b'ic10', 1024)]:
    image = png(size)
    payload += kind + struct.pack('>I', len(image) + 8) + image
(OUT / 'icon.icns').write_bytes(b'icns' + struct.pack('>I', len(payload) + 8) + payload)
