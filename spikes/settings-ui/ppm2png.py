#!/usr/bin/env python3
"""ppm2png.py — convert a binary PPM (P6) framebuffer dump to PNG. Stdlib only (zlib).

The app dumps the virtual framebuffer as raw P6 PPM (trivial, no deps under qemu); the host
turns it into the PNG artifact the bead/CI wants. No libpng, no Pillow.
  ppm2png.py in.ppm out.png

PROVENANCE (tsp-ht0p.1): copied verbatim from pocketforge-os/sim @ 74ddfbc
sim/fb/ppm2png.py so this spike stays self-contained inside runtime/. Re-copy
from there if the upstream changes; do NOT hand-edit either copy.
"""
import struct
import sys
import zlib


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[:2] != b"P6":
        raise ValueError(f"{path}: not a P6 PPM")
    i, vals = 2, []
    while len(vals) < 3:
        while i < len(data) and data[i] in b" \t\n\r":
            i += 1
        if data[i:i + 1] == b"#":
            while data[i] not in b"\n":
                i += 1
            continue
        s = i
        while data[i] not in b" \t\n\r":
            i += 1
        vals.append(int(data[s:i]))
    w, h, _maxv = vals
    i += 1  # single whitespace after maxval
    return w, h, data[i:i + w * h * 3]


def read_png(path):
    """Decode an 8-bit, non-interlaced PNG (colour type 2 RGB or 6 RGBA) to (w, h, rgb-bytes).

    Stdlib only (zlib) — the read counterpart of write_png, added for tsp-an4.6 so the bezel
    skins (PIL-written 8-bit RGB, adaptive per-row filters) decode on hosts with NO Pillow (mm).
    Handles all five PNG filter types (None/Sub/Up/Average/Paeth); alpha is dropped to RGB.
    """
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path}: not a PNG")
    off, w = 8, None
    idat = bytearray()
    while off < len(data):
        ln = struct.unpack(">I", data[off:off + 4])[0]
        typ = data[off + 4:off + 8]
        body = data[off + 8:off + 8 + ln]
        if typ == b"IHDR":
            w, h, bitdepth, colortype, comp, filt, interlace = struct.unpack(">IIBBBBB", body)
            if bitdepth != 8 or colortype not in (2, 6) or interlace != 0:
                raise ValueError(f"{path}: unsupported PNG (bitdepth={bitdepth} "
                                 f"colortype={colortype} interlace={interlace})")
        elif typ == b"IDAT":
            idat += body
        elif typ == b"IEND":
            break
        off += 12 + ln
    if w is None:
        raise ValueError(f"{path}: no IHDR")
    chans = 4 if colortype == 6 else 3
    raw = zlib.decompress(bytes(idat))
    stride = w * chans
    out = bytearray(h * w * 3)
    prev = bytearray(stride)
    pos = 0
    for y in range(h):
        ft = raw[pos]; pos += 1
        line = bytearray(raw[pos:pos + stride]); pos += stride
        if ft == 1:      # Sub
            for i in range(chans, stride):
                line[i] = (line[i] + line[i - chans]) & 0xff
        elif ft == 2:    # Up
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xff
        elif ft == 3:    # Average
            for i in range(stride):
                a = line[i - chans] if i >= chans else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xff
        elif ft == 4:    # Paeth
            for i in range(stride):
                a = line[i - chans] if i >= chans else 0
                b = prev[i]
                c = prev[i - chans] if i >= chans else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xff
        # type 0 (None) needs no reconstruction
        o = y * w * 3
        if chans == 3:
            out[o:o + stride] = line
        else:                            # drop alpha
            for x in range(w):
                s = x * 4
                out[o + x * 3:o + x * 3 + 3] = line[s:s + 3]
        prev = line
    return w, h, bytes(out)


def png_dims(path):
    """Read just (w, h) from a PNG's IHDR — cheap, no decode."""
    with open(path, "rb") as f:
        head = f.read(24)
    if head[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path}: not a PNG")
    return struct.unpack(">II", head[16:24])


def write_ppm(path, w, h, rgb):
    """Write raw P6 PPM — the format the SDL3 renderer (skin-render.c) reads (no PIL/SDL_image)."""
    with open(path, "wb") as f:
        f.write(b"P6\n%d %d\n255\n" % (w, h))
        f.write(bytes(rgb))


def write_png(path, w, h, rgb):
    def chunk(typ, b):
        return struct.pack(">I", len(b)) + typ + b + struct.pack(">I", zlib.crc32(typ + b) & 0xffffffff)
    raw = bytearray()
    for y in range(h):
        raw.append(0)                        # filter: none
        raw += rgb[y * w * 3:(y + 1) * w * 3]
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)   # 8-bit RGB
    png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
           + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b""))
    with open(path, "wb") as f:
        f.write(png)


def main(argv):
    if len(argv) != 2:
        sys.exit("usage: ppm2png.py in.ppm out.png")
    w, h, rgb = read_ppm(argv[0])
    write_png(argv[1], w, h, rgb)
    print(f"{argv[1]} ({w}x{h})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
