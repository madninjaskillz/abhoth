"""Cut the mascot's six expression variants (1254x1254 PNGs on opaque black, named
<mood>.png in SRC_DIR) out of their background and frame them like
client/src/assets/brand/abhoth-mascot-256.webp, so swapping moods does not move the character.

    python tools/brand/cut_moods.py SRC_DIR client/src/assets/brand/moods

The outline is a dark purple (~29,7,40) on pure black (0..1), so alpha comes from the pixel's
brightness right at the edge and is solid inside."""
import sys, os
import numpy as np
from PIL import Image
from scipy import ndimage

src_dir, out_dir = sys.argv[1], sys.argv[2]
os.makedirs(out_dir, exist_ok=True)
MOODS = ["happy", "surprised", "angry", "sad", "wink", "sleepy"]
# Same framing as the existing mascot-256: content x 1..254, y 19..236 of 256.
SRC_BOX = (36, 246, 1150, 1201)  # union content box of all six (x0, y0, x1, y1), measured
REF_X0, REF_X1, REF_Y0 = 1, 254, 19

def cut(path):
    a = np.array(Image.open(path).convert("RGB")).astype(np.float32)
    mx = a.max(axis=2)
    dark = mx <= 3.0
    # The background: near-black regions touching the border, plus any large enclosed
    # see-through gap. Small near-black specks inside (eye shadows, line noise) stay opaque.
    lab, n = ndimage.label(dark)
    sizes = ndimage.sum(dark, lab, index=np.arange(1, n + 1))
    border = set(np.unique(np.concatenate([lab[0], lab[-1], lab[:, 0], lab[:, -1]]))) - {0}
    bg_ids = np.zeros(n + 1, bool)
    for i in range(1, n + 1):
        if i in border or sizes[i - 1] >= 2000:
            bg_ids[i] = True
    background = bg_ids[lab]
    # Drop stray specks floating in the background (none expected).
    fg = ~background
    flab, fn = ndimage.label(fg)
    fsizes = ndimage.sum(fg, flab, index=np.arange(1, fn + 1))
    fkeep = np.zeros(fn + 1, bool)
    fkeep[1:] = fsizes >= 40
    fg = fkeep[flab]
    # Edge ring (2 px next to the background): alpha from brightness over the outline's level.
    ring = fg & ~ndimage.binary_erosion(fg, iterations=2, border_value=1)
    alpha = np.where(fg, 1.0, 0.0)
    alpha[ring] = np.clip(mx[ring] / 26.0, 0.0, 1.0)
    # Edge pixels are the outline mixed with black: un-mix the colour.
    rgb = a.copy()
    edge = (alpha > 0) & (alpha < 1)
    rgb[edge] = np.clip(a[edge] / alpha[edge][:, None], 0, 255)
    out = np.dstack([rgb, alpha * 255.0]).round().astype(np.uint8)
    return Image.fromarray(out, "RGBA")

def frame(im, size):
    x0, y0, x1, y1 = SRC_BOX
    s = (REF_X1 - REF_X0) / (x1 - x0)  # canvas px per source px at 256
    # Source rectangle that maps onto the 256 canvas.
    sx0 = x0 - REF_X0 / s
    sy0 = y0 - REF_Y0 / s
    span = 256 / s
    pm = im.convert("RGBa")
    pad = 400
    big = Image.new("RGBa", (im.width + 2 * pad, im.height + 2 * pad), (0, 0, 0, 0))
    big.paste(pm, (pad, pad))
    box = (sx0 + pad, sy0 + pad, sx0 + pad + span, sy0 + pad + span)
    return big.resize((size, size), Image.LANCZOS, box=box).convert("RGBA")

for m in MOODS:
    full = cut(os.path.join(src_dir, f"{m}.png"))
    for size in (64, 128, 256):
        fr = frame(full, size)
        fr.save(os.path.join(out_dir, f"abhoth-mood-{m}-{size}.webp"), "WEBP", quality=90, alpha_quality=100, method=6)
    print(m, "ok")
