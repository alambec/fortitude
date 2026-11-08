#!/usr/bin/env bash
# Regenerate the JPEG fixtures: synthetic images encoded with libjpeg-turbo's cjpeg, and the
# samples its djpeg decodes them to (PGM/PPM, 16-bit big endian above 8 bits), which the
# decoder of fortitude-store must reproduce exactly.
#
# Needs libjpeg-turbo 3 (12-bit support in cjpeg and djpeg), e.g. built from source:
#   cmake -S libjpeg-turbo -B build && make -C build cjpeg djpeg
#   CJPEG=build/cjpeg DJPEG=build/djpeg crates/fortitude-testkit/data/jpeg/generate.sh
set -euo pipefail
cd "$(dirname "$0")"
cjpeg=${CJPEG:-cjpeg}
djpeg=${DJPEG:-djpeg}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Gradients, waves, a sharp-edged square and noise, reaching both ends of the sample range.
python3 - "$work" <<'PY'
import math, sys
work = sys.argv[1]
def image(path, width, height, channels, bits):
    top = (1 << bits) - 1
    seed = 12345
    out = bytearray()
    for y in range(height):
        for x in range(width):
            for c in range(channels):
                seed = (seed * 1103515245 + 12345) & 0x7FFFFFFF
                noise = (seed >> 16) % 61 - 30
                v = (x * (c + 1) * 70 + y * 40) % (top + 1) * 0.6
                v += 0.25 * top * math.sin((x + 3 * c) / 4.0) * math.cos(y / 5.0)
                if 10 <= x < 30 and 8 <= y < 25:
                    v = top if c != 1 else 0
                v = min(top, max(0, int(v + noise * top / 4095)))
                out += v.to_bytes(2, "big") if bits > 8 else bytes([v])
    magic = "P5" if channels == 1 else "P6"
    with open(path, "wb") as f:
        f.write(f"{magic}\n{width} {height}\n{top}\n".encode() + bytes(out))
image(f"{work}/gray12.pgm", 61, 45, 1, 12)
image(f"{work}/color12.ppm", 61, 45, 3, 12)
image(f"{work}/narrow12.ppm", 3, 5, 3, 12)
image(f"{work}/color8.ppm", 61, 45, 3, 8)
PY
# Sequential scans, one per component (non-interleaved).
printf '0;\n1;\n2;\n' > "$work/scans.txt"

encode() { # name source cjpeg-options...
    local name=$1 source=$2
    shift 2
    "$cjpeg" "$@" -outfile "$name.jpg" "$work/$source"
    local ext=pgm
    [[ $source == *.ppm ]] && ext=ppm
    "$djpeg" -outfile "$name.$ext" "$name.jpg"
}
encode 12-gray gray12.pgm -precision 12 -quality 90
encode 12-gray-restart gray12.pgm -precision 12 -quality 10 -restart 3B
encode 12-ycbcr-420 color12.ppm -precision 12 -quality 85 -sample 2x2 -restart 1
encode 12-ycbcr-422 color12.ppm -precision 12 -quality 85 -sample 2x1
encode 12-ycbcr-440 color12.ppm -precision 12 -quality 85 -sample 1x2
encode 12-ycbcr-411 color12.ppm -precision 12 -quality 85 -sample 4x1
encode 12-ycbcr-444 color12.ppm -precision 12 -quality 95 -sample 1x1
encode 12-ycbcr-scans color12.ppm -precision 12 -quality 85 -sample 2x2 -scans "$work/scans.txt"
encode 12-ycbcr-420-narrow narrow12.ppm -precision 12 -quality 85 -sample 2x2
encode 12-rgb color12.ppm -precision 12 -quality 85 -rgb
encode 8-ycbcr-420 color8.ppm -quality 85 -sample 2x2 -restart 2B
