"""Wraps the right side of the Rust logo's gear around the wordmark's R.

The gear is the Rust artwork's, in its own orientation and proportions, but open: its band is an arc
with rounded ends where it meets the top and bottom of the O, and only the cogs, mounts and holes on
that arc are drawn. The R keeps its size and place; the gear is centred on it, behind the letters.

Usage: python3 gearopen.py <wordmark.svg> <out.svg> <gear colour>
"""
import math
import re
import sys

src, out, colour = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(src).read()

# The gear's own units: band r 43 width 9 (inner edge 38.5), cogs to r 51, mounts to r 35, holes at r 40.
BAND, BAND_WIDTH, INNER, OUTER = 43.0, 9.0, 38.5, 51.0
R_HALF_HEIGHT = 35.0
SCALE = (R_HALF_HEIGHT + 5.0) / INNER
COG_HALF_ANGLE = math.degrees(math.atan2(3, 46)) + 2.0
MOUNT_HALF_ANGLE = math.degrees(math.atan2(7, 42)) + 2.0


def bbox(d):
    toks = re.findall(r'[MLHVQZ]|-?\d*\.?\d+(?:e-?\d+)?', d)
    xs, ys, i, cmd, x, y = [], [], 0, None, 0.0, 0.0
    while i < len(toks):
        t = toks[i]
        if t.isalpha():
            cmd, i = t, i + 1
            continue
        if cmd in 'ML':
            x, y, i = float(toks[i]), float(toks[i + 1]), i + 2
        elif cmd == 'H':
            x, i = float(toks[i]), i + 1
        elif cmd == 'V':
            y, i = float(toks[i]), i + 1
        elif cmd == 'Q':
            x, y, i = float(toks[i + 2]), float(toks[i + 3]), i + 4
        xs.append(x); ys.append(y)
    return min(xs), min(ys), max(xs), max(ys)


paths = list(re.finditer(r'<path fill="([^"]+)" d="([^"]+)"/>', s))
text_colour = paths[0].group(1)
letters = [p for p in paths if p.group(1) == text_colour]
o_box = bbox(letters[2].group(2))
r_path = letters[3]
x0, y0, x1, y1 = bbox(r_path.group(2))
cx, cy = (x0 + x1) / 2, (y0 + y1) / 2

# The band ends where it meets the O's top and bottom, on the gear's left side.
band = BAND * SCALE
end_dy = min(abs(o_box[1] - cy), band - 0.01)
end = math.degrees(math.atan2(end_dy, -math.sqrt(band * band - end_dy * end_dy)))
# Angles run from -end (top left) through 0 (right) to +end (bottom left), y pointing down.


def on_arc(angle, half):
    wrapped = (angle + 180.0) % 360.0 - 180.0
    return -end + half <= wrapped <= end - half


def point(angle, radius):
    a = math.radians(angle)
    return radius * math.cos(a), radius * math.sin(a)


sx, sy = point(-end, BAND)
ex, ey = point(end, BAND)
arc = (
    f'<path d="M{sx:.3f} {sy:.3f} A{BAND} {BAND} 0 1 1 {ex:.3f} {ey:.3f}" fill="none" '
    f'stroke-width="{BAND_WIDTH}" stroke-linecap="round"/>'
)
cogs = ''.join(
    f'<polygon stroke-width="3" points="46,3 51,0 46,-3" transform="rotate({i * 11.25})"/>'
    for i in range(32)
    if on_arc(i * 11.25, COG_HALF_ANGLE)
)
# The mounts and holes point up at rotation 0, which is -90 degrees here.
mount_angles = [k * 72 for k in range(5) if on_arc(k * 72 - 90, MOUNT_HALF_ANGLE)]
mounts = ''.join(f'<polygon stroke-width="6" points="-7,-42 0,-35 7,-42" transform="rotate({a})"/>' for a in mount_angles)
holes = ''.join(f'<circle cy="-40" r="3" fill="#000" transform="rotate({a})"/>' for a in mount_angles)
gear = (
    f'<g transform="translate({cx:.2f} {cy:.2f}) scale({SCALE:.4f})">'
    '<mask id="ankor-gear-holes"><rect x="-60" y="-60" width="120" height="120" fill="#fff"/>'
    f'{holes}</mask>'
    f'<g mask="url(#ankor-gear-holes)" fill="{colour}" stroke="{colour}" stroke-linejoin="round">'
    f'{arc}{cogs}{mounts}'
    '</g></g>'
)

first = letters[0]
result = s[: first.start()] + gear + s[first.start(): r_path.end()]
radius = OUTER * SCALE
right = cx + radius
shift = max(0.0, right + 2.0 - x1)
tail = s[r_path.end():]
if shift:
    tail = re.sub(r'<path fill="([^"]+)" d="([^"]+)"/>', lambda m: f'<g transform="translate({shift:.2f} 0)">{m.group(0)}</g>', tail)
result += tail

top = min(0.0, cy - radius - 1.0)
bottom = max(100.0, cy + radius + 1.0)
width = float(re.search(r'viewBox="0 0 ([\d.]+) 100"', s).group(1))
new_width = max(width + shift, right + 2.0)
result = result.replace(f'viewBox="0 0 {width:g} 100"', f'viewBox="0 {top:.1f} {new_width:.1f} {bottom - top:.1f}"', 1)
open(out, 'w').write(result)
print(out, f'arc ±{end:.1f}°', 'mounts at', mount_angles, f'viewBox 0 {top:.1f} {new_width:.1f} {bottom - top:.1f}')
