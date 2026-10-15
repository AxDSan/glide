#!/usr/bin/env python3
"""Generate the bundled cursor themes with fal.ai.

Each cursor is generated with Meta Muse Image ($0.01 per image) as a glossy 3D
icon, cut out with BiRefNet (about $0.002), cleaned of stray specks, trimmed, and
given a hotspot by its kind. Writes assets/cursors/<theme>/<kind>.png and
assets/cursors/manifest.json.

Usage: FAL_KEY=... scripts/gen_cursors.py [--new] [--reprocess] [theme[/kind] ...]
Without arguments every missing image is generated; name a theme or theme/kind
to rebuild only that. Cut-outs are cached in target/cursor-src; --new generates
fresh ones, --reprocess rebuilds every PNG from the cache without any API calls.
--budget=DOLLARS (default 0.55) caps the estimated spend; jobs past it are skipped.

The geometric kinds (ibeam, crosshair, resize arrows) are drawn by the renderer
in each theme's colours, so they are not generated here.
"""

import json
import os
import io
import sys
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import numpy as np
from PIL import Image
from scipy import ndimage

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "assets" / "cursors"
# Cut-outs, so images can be reprocessed without paying for new generations.
CACHE = ROOT / "target" / "cursor-src"
GENERATE = "https://fal.run/meta/muse-image/text-to-image"
# Estimated dollars per cursor: one generation plus one cut-out.
COST = 0.012
CUT_OUT = "https://fal.run/fal-ai/birefnet/v2"
# Longest side in pixels of a finished cursor image.
HEIGHT = 192
PAD = 4

SUFFIX = (
    ", glossy 3D icon, soft studio lighting, smooth gradients, specular highlights, "
    "depth and bevel, large and filling the frame, a single centered object, isolated "
    "on a plain white background, no text, no ground shadow, nothing else in the image"
)

PIXEL_SUFFIX = (
    ", retro 16-bit pixel art sprite, crisp hard-edged square pixels, limited palette, "
    "black pixel outline, large and filling the frame, a single centered object, isolated "
    "on a plain white background, no text, nothing else in the image"
)

# Kind: (shape description, hotspot rule).
# Rules: "tl" = the opaque pixel nearest the top-left corner, "top" = topmost
# opaque pixel, "center" = centre of the bounding box.
CLASSIC_KINDS = {
    "arrow": ("a mouse pointer arrow cursor pointing diagonally up-left, its sharp tip in the top-left", "tl"),
    "pointing-hand": ("a hand cursor with the index finger pointing straight up, other fingers folded", "top"),
    "open-hand": ("an open hand cursor, palm facing the viewer, fingers slightly spread pointing up", "center"),
    "closed-hand": ("a closed grabbing hand cursor, fingers curled in a grip", "center"),
}

THEMES = {
    "macos": {
        "look": "polished piano-black desktop cursor with a crisp white rim",
        "kinds": CLASSIC_KINDS,
        "scale": 1.0,
    },
    "windows": {
        "look": "pearl-white desktop cursor with a thin dark rim",
        "kinds": CLASSIC_KINDS,
        "scale": 1.0,
    },
    "flat": {
        "look": "soft matte clay style, slate blue, rounded edges",
        "kinds": CLASSIC_KINDS,
        "scale": 1.0,
    },
    "playful": {
        "look": "glossy candy style, vivid purple and yellow, bubbly and rounded",
        "kinds": CLASSIC_KINDS,
        "scale": 1.1,
    },
    "pixel": {
        "look": "classic retro video game cursor, bright golden-yellow fill with a black outline and a white highlight",
        "kinds": CLASSIC_KINDS,
        "scale": 1.1,
        "suffix": PIXEL_SUFFIX,
    },
    "guns": {
        "look": "colorful toy sci-fi blaster style",
        "kinds": {
            "arrow": ("a ray gun blaster pistol, its muzzle pointing diagonally up-left with the muzzle tip in the top-left", "tl"),
            "pointing-hand": ("a laser rifle pointing straight up, muzzle at the top", "top"),
            "open-hand": ("an armored power glove, open hand, fingers spread", "center"),
            "closed-hand": ("an armored power glove clenched into a fist", "center"),
        },
        "scale": 1.4,
    },
    "items": {
        "look": "fantasy video game item style",
        "kinds": {
            "arrow": ("a sword with its blade pointing diagonally up-left, blade tip in the top-left, hilt in the bottom-right", "tl"),
            "pointing-hand": ("a magic wand pointing straight up with a glowing star at its top", "top"),
            "open-hand": ("an open treasure chest full of gold coins", "center"),
            "closed-hand": ("a closed wooden treasure chest", "center"),
        },
        "scale": 1.4,
    },
    "animals": {
        "look": "cute animal emoji style, warm colors",
        "kinds": {
            "arrow": ("a little bird flying diagonally up-left, its beak tip in the top-left", "tl"),
            "pointing-hand": ("a cat paw with one toe raised pointing straight up", "top"),
            "open-hand": ("an open cat paw showing pink toe beans", "center"),
            "closed-hand": ("a curled cat paw seen from the side", "center"),
        },
        "scale": 1.4,
    },
    "chibi": {
        "look": "chibi figurine style, big cute eyes, pastel colors",
        "kinds": {
            "arrow": ("an arrow cursor character with a tiny cute face, pointing diagonally up-left, its tip in the top-left", "tl"),
            "pointing-hand": ("a small chibi hand with the index finger pointing straight up and a sleeve cuff", "top"),
            "open-hand": ("a small chibi open hand waving", "center"),
            "closed-hand": ("a small chibi fist", "center"),
        },
        "scale": 1.4,
    },
    "cute": {
        "look": "kawaii style, pastel pink and lavender with tiny hearts",
        "kinds": {
            "arrow": ("an arrow cursor pointing diagonally up-left, its tip in the top-left", "tl"),
            "pointing-hand": ("a hand with the index finger pointing straight up holding a tiny heart", "top"),
            "open-hand": ("an open hand with a small heart on the palm", "center"),
            "closed-hand": ("a closed hand holding a small heart", "center"),
        },
        "scale": 1.4,
    },
}


def call(url: str, body: dict, key: str) -> dict:
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={
        "Authorization": f"Key {key}",
        "Content-Type": "application/json",
    })
    with urllib.request.urlopen(req, timeout=180) as r:
        return json.load(r)


def generate(prompt: str, key: str) -> bytes:
    """A cut-out PNG (RGBA) of a freshly generated image."""
    img = call(GENERATE, {"prompt": prompt, "aspect_ratio": "1:1", "output_format": "png"}, key)["images"][0]["url"]
    cut = call(CUT_OUT, {"image_url": img, "model": "General Use (Light)", "output_format": "png"}, key)
    with urllib.request.urlopen(cut["image"]["url"], timeout=60) as r:
        return r.read()


def finish(rgba: np.ndarray, rule: str):
    alpha = rgba[..., 3] > 127
    labels, n = ndimage.label(alpha)
    if n == 0:
        raise ValueError("empty image after cut-out")
    areas = ndimage.sum(alpha, labels, range(1, n + 1))
    main = labels == (int(np.argmax(areas)) + 1)
    # Drop specks and stray marks (motion lines, sparkles far from the shape).
    for i, area in enumerate(areas, start=1):
        if area < 0.03 * areas.max():
            rgba[labels == i, 3] = 0
    alpha = rgba[..., 3] > 127
    ys, xs = np.nonzero(alpha)
    if rule == "tl":
        # The tip should be the topmost point on the left; mirror shapes that face right.
        my, mx = np.nonzero(main)
        top_x = mx[my == my.min()].mean()
        if top_x > xs.min() + 0.6 * (xs.max() - xs.min()):
            rgba = rgba[:, ::-1].copy()
            main = main[:, ::-1]
            ys, xs = np.nonzero(rgba[..., 3] > 127)
    y0, y1, x0, x1 = ys.min(), ys.max() + 1, xs.min(), xs.max() + 1
    crop = rgba[y0:y1, x0:x1]
    ys, xs = np.nonzero(main[y0:y1, x0:x1])
    if rule == "tl":
        i = np.argmin(xs + ys)
        hot = (xs[i] + 0.5, ys[i] + 0.5)
    elif rule == "top":
        top = ys.min()
        row = xs[ys == top]
        hot = (row.mean() + 0.5, top + 0.5)
    else:
        hot = (crop.shape[1] / 2, crop.shape[0] / 2)
    img = Image.fromarray(crop, "RGBA")
    inner = HEIGHT - 2 * PAD
    k = inner / max(img.width, img.height)
    img = img.resize((max(1, round(img.width * k)), max(1, round(img.height * k))), Image.LANCZOS)
    canvas = Image.new("RGBA", (img.width + 2 * PAD, img.height + 2 * PAD))
    canvas.paste(img, (PAD, PAD))
    hx = (PAD + hot[0] * k) / canvas.width
    hy = (PAD + hot[1] * k) / canvas.height
    return canvas, (round(float(hx), 4), round(float(hy), 4))


def main():
    key = os.environ.get("FAL_KEY", "")
    if not key and "--reprocess" not in sys.argv:
        sys.exit("set FAL_KEY")
    args = sys.argv[1:]
    # --reprocess: rebuild PNGs from cached SVGs only; --new: ignore the cache.
    regenerate = "--new" in args
    reprocess = "--reprocess" in args
    budget = next((float(a.split("=", 1)[1]) for a in args if a.startswith("--budget=")), 0.55)
    wanted = [a for a in args if not a.startswith("--")]
    manifest_path = OUT / "manifest.json"
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}

    jobs = []
    for theme, spec in THEMES.items():
        for kind, (shape, rule) in spec["kinds"].items():
            name = f"{theme}/{kind}"
            png = OUT / theme / f"{kind}.png"
            selected = any(w in (theme, name) for w in wanted)
            if (wanted and not selected) or (not wanted and png.exists() and not reprocess):
                continue
            if reprocess and not (CACHE / theme / f"{kind}.png").exists():
                continue
            prompt = f"{shape}, {spec['look']}{spec.get('suffix', SUFFIX)}"
            jobs.append((theme, kind, rule, prompt, png, spec["scale"]))

    paid = [j for j in jobs if regenerate or not (CACHE / j[0] / f"{j[1]}.png").exists()]
    allowed = int(budget / COST + 1e-9)
    if len(paid) > allowed:
        skipped = {(j[0], j[1]) for j in paid[allowed:]}
        print(f"budget ${budget:.2f} covers {allowed} of {len(paid)} generations; skipping the rest")
        jobs = [j for j in jobs if (j[0], j[1]) not in skipped]
        paid = paid[:allowed]
    print(f"generating {len(paid)} (about ${len(paid) * COST:.2f}), reprocessing {len(jobs) - len(paid)}")

    def run(job):
        theme, kind, rule, prompt, png, scale = job
        try:
            cached = CACHE / theme / f"{kind}.png"
            if cached.exists() and not regenerate:
                src = cached.read_bytes()
            else:
                src = generate(prompt, key)
                cached.parent.mkdir(parents=True, exist_ok=True)
                cached.write_bytes(src)
            rgba = np.asarray(Image.open(io.BytesIO(src)).convert("RGBA")).copy()
            img, hot = finish(rgba, rule)
            png.parent.mkdir(parents=True, exist_ok=True)
            img.save(png, optimize=True)
            print(f"ok   {theme}/{kind} hotspot={hot}", flush=True)
            return theme, kind, {"hotspot": list(hot), "scale": scale}
        except Exception as e:  # noqa: BLE001 - report and continue with the rest
            print(f"FAIL {theme}/{kind}: {e}", flush=True)
            return None

    with ThreadPoolExecutor(6) as pool:
        for res in pool.map(run, jobs):
            if res:
                theme, kind, entry = res
                manifest.setdefault(theme, {})[kind] = entry
    OUT.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
