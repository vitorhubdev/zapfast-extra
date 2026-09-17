#!/usr/bin/env python3
"""Builds the ZapExt icon assets from the master logo artwork.

Usage:
    python scripts/make-icons.py --source path/to/logo.png

Outputs:
    assets/zapext.png             1024x1024 RGBA (window, tray, README)
    packaging/macos/icon-1024.png same artwork for the macOS bundle
    packaging/windows/zapfast.ico 16/24/32/48/64/128/256 frames for the executable
    packaging/icons/zapfast.svg   vector trace for Linux and Flatpak desktop icons

The master artwork is a rounded green square with a ribbon Z, speed lines and a
double plus. The script masks the corners, resamples to the icon sizes above,
and traces the artwork into SVG paths so every surface shows the same logo:
the background becomes a measured linear gradient, the shaded faces are
painted flat, and the white and mint faces carry their own gradients.
"""

import argparse
import math
import sys
from collections import defaultdict
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter
from scipy.ndimage import gaussian_filter, label

ROOT = Path(__file__).resolve().parent.parent
ICON_SIZES = (16, 24, 32, 48, 64, 128, 256)
VECTOR_SIDE = 512
BANDS = 20
MIN_BAND_PIXELS = 150


def load_source(path):
    image = Image.open(path).convert("RGBA")
    return np.asarray(image)[:, :, :3].astype(np.float32)


def find_bounds(rgb):
    """Bounds of the visible artwork, ignoring the black matte around it."""
    luminance = rgb.mean(axis=2)
    ys, xs = np.where(luminance > 40)
    return int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max())


def coverage_mask(width, height, radius, supersample=4):
    """Antialiased rounded-square alpha for a width x height square."""
    yy, xx = np.mgrid[0:height, 0:width].astype(np.float32)
    acc = np.zeros((height, width), dtype=np.float32)
    step = 1.0 / supersample
    offset = step / 2.0
    for sy in range(supersample):
        for sx in range(supersample):
            px = xx + offset + sx * step
            py = yy + offset + sy * step
            dx = np.maximum(np.maximum(radius - px, px - (width - 1 - radius)), 0.0)
            dy = np.maximum(np.maximum(radius - py, py - (height - 1 - radius)), 0.0)
            acc += (dx * dx + dy * dy) <= radius * radius
    return acc / (supersample * supersample)


def find_radius(rgb, x0, y0, x1, y1):
    """Corner radius that best fits the rounded square, in source pixels."""
    visible = rgb.mean(axis=2) > 40
    side = min(x1 - x0 + 1, y1 - y0 + 1)
    best_radius, best_error = int(side * 0.22), 1e18
    for radius in range(int(side * 0.12), int(side * 0.40)):
        error = 0.0
        samples = 0
        for dy in range(0, radius, max(1, radius // 48)):
            if dy + y0 >= visible.shape[0]:
                break
            expected = radius - math.sqrt(max(radius * radius - (radius - dy) ** 2, 0.0))
            row = visible[y0 + dy, x0 : x0 + radius + 2]
            if not row.any():
                continue
            actual = float(np.argmax(row))
            error += (actual - expected) ** 2
            samples += 1
        if samples and error / samples < best_error:
            best_radius, best_error = radius, error / samples
    return best_radius


def icon_rgba(rgb, x0, y0, x1, y1, radius, size, soften=False):
    """Resamples the artwork to size x size with transparent rounded corners.

    Resampling leaves faint one-step noise across the gradients, which costs
    hundreds of kilobytes in the embedded PNG. A sub-pixel softening removes it
    without a visible change, and the silhouette keeps its own coverage mask.
    """
    crop = rgb[y0 : y1 + 1, x0 : x1 + 1]
    alpha = coverage_mask(crop.shape[1], crop.shape[0], float(radius))
    color = Image.fromarray(crop.astype(np.uint8), "RGB").resize(
        (size, size), Image.LANCZOS
    )
    if soften:
        color = color.filter(ImageFilter.BoxBlur(0.6))
    mask = Image.fromarray((alpha * 255.0).astype(np.uint8), "L").resize(
        (size, size), Image.LANCZOS
    )
    rgba = np.dstack([np.asarray(color), np.asarray(mask)]).astype(np.uint8)
    return Image.fromarray(rgba, "RGBA")


def diagonal(shape):
    """Position along the top-left to bottom-right diagonal, 0 to 1."""
    height, width = shape[:2]
    yy, xx = np.mgrid[0:height, 0:width].astype(np.float32)
    return (xx + yy) / (2.0 * max(width - 1, height - 1, 1))


def curve_at(samples, u):
    """Piecewise linear background colour at position u."""
    positions = np.array([sample[0] for sample in samples])
    values = np.stack([sample[1] for sample in samples])
    return np.stack(
        [np.interp(u, positions, values[:, channel]) for channel in range(3)], axis=-1
    )


def background_samples(rgb, coverage, u, artwork):
    """Gradient stops measured on pixels that look like the background."""
    inside = (coverage > 0.995) & ~artwork
    flat = rgb.reshape(-1, 3)
    positions = u.ravel().astype(np.float64)
    usable = inside.ravel()
    low_edge = float(np.quantile(positions[usable], 0.04))
    high_edge = float(np.quantile(positions[usable], 0.96))
    start = flat[usable & (positions <= low_edge)].mean(axis=0)
    end = flat[usable & (positions >= high_edge)].mean(axis=0)
    line = start[None, :] + (end - start)[None, :] * np.clip(
        (positions - low_edge) / (high_edge - low_edge), 0.0, 1.0
    )[:, None]
    # Artwork edges are blends; keep only pixels that sit on the straight line.
    plausible = usable & (np.abs(flat - line).sum(axis=1) < 45.0)
    samples = [(low_edge, start), (high_edge, end)]
    for index in range(BANDS):
        low, high = index / BANDS, (index + 1) / BANDS
        band = plausible & (positions >= low) & (positions < high)
        found = np.flatnonzero(band)
        if found.size < MIN_BAND_PIXELS:
            continue
        samples.append((float(positions[found].mean()), np.median(flat[found], axis=0)))
    samples.sort(key=lambda sample: sample[0])
    return tidy_stops(samples)


def tidy_stops(samples):
    """Smooths the measured stops and keeps them bright to dark."""
    positions = [position for position, _ in samples]
    values = np.stack([color for _, color in samples])
    smoothed = values.copy()
    for index in range(1, len(values) - 1):
        smoothed[index] = (
            0.25 * values[index - 1] + 0.5 * values[index] + 0.25 * values[index + 1]
        )
    for index in range(1, len(smoothed)):
        smoothed[index] = np.minimum(smoothed[index], smoothed[index - 1])
    return list(zip(positions, smoothed))


def artwork_core(rgb, samples, coverage, u, threshold=60.0, min_area=120):
    """Pixels that differ from the background gradient, noise removed."""
    delta = np.abs(rgb - curve_at(samples, u)).sum(axis=2)
    core = (delta > threshold) & (coverage > 0.5)
    labels, count = label(core)
    if count:
        sizes = np.bincount(labels.ravel())
        keep = np.zeros(sizes.shape, dtype=bool)
        keep[1:] = sizes[1:] >= min_area
        core = keep[labels]
    return core & (coverage > 0.5)


def artwork_layers(rgb, core, predicted):
    """Splits the artwork into shadow, mint and white layers.

    Antialiased edges are brighter than the background and belong to no layer:
    painting them as shaded faces would rim every shape with a dark halo.
    """
    bright = rgb.min(axis=2)
    spread = rgb.max(axis=2) - rgb.min(axis=2)
    green = rgb[..., 1]
    blue = rgb[..., 2]
    white = core & (bright > 205.0) & (spread < 55.0)
    mint = core & ~white & (green > 145.0) & ((green - blue) > 12.0) & (bright > 75.0)
    shaded = (predicted - rgb).sum(axis=2) > 35.0
    shadow = core & ~white & ~mint & shaded
    return white, mint, shadow


def face_gradient(values):
    """Light and shaded ends of a face, for its own gradient."""
    return (
        np.percentile(values, 92, axis=0),
        np.percentile(values, 8, axis=0),
    )


def vignette_opacity(rgb, samples, coverage, artwork, u):
    """Darkening near the edges that the diagonal gradient misses."""
    inside = (coverage > 0.995) & ~artwork
    if int(inside.sum()) < 64:
        return 0.0
    error = curve_at(samples, u) - rgb
    height, width = rgb.shape[:2]
    yy, xx = np.mgrid[0:height, 0:width].astype(np.float32)
    radius = np.hypot(xx - (width - 1) / 2.0, yy - (height - 1) / 2.0)
    radius /= radius.max()
    outer = inside & (radius > 0.82)
    if int(outer.sum()) < 32:
        return 0.0
    darkening = float(np.clip(error[outer].mean(), 0.0, 255.0)) / 255.0
    return round(darkening, 3)


def boundary_loops(mask):
    """Closed pixel-boundary loops of a boolean mask, holes included."""
    inside = np.zeros((mask.shape[0] + 2, mask.shape[1] + 2), dtype=bool)
    inside[1:-1, 1:-1] = mask
    edges = defaultdict(list)

    for y, x in zip(*[array.tolist() for array in np.where(inside)]):
        if not inside[y - 1, x]:
            edges[(x, y)].append((x + 1, y))
        if not inside[y, x + 1]:
            edges[(x + 1, y)].append((x + 1, y + 1))
        if not inside[y + 1, x]:
            edges[(x + 1, y + 1)].append((x, y + 1))
        if not inside[y, x - 1]:
            edges[(x, y + 1)].append((x, y))

    loops = []
    for start in list(edges.keys()):
        while edges.get(start):
            loop = [start]
            current = start
            while True:
                outgoing = edges.get(current)
                if not outgoing:
                    break
                nxt = outgoing.pop()
                if not outgoing:
                    del edges[current]
                loop.append(nxt)
                current = nxt
                if current == start:
                    break
            if len(loop) > 3:
                loops.append([(x - 1.0, y - 1.0) for x, y in loop[:-1]])
    return loops


def polygon_area(points):
    total = 0.0
    for index in range(len(points)):
        ax, ay = points[index]
        bx, by = points[(index + 1) % len(points)]
        total += ax * by - bx * ay
    return abs(total) / 2.0


def rdp(points, epsilon):
    """Ramer-Douglas-Peucker simplification of a closed polygon."""
    if len(points) < 4:
        return points
    closed = points + [points[0]]
    count = len(points)
    keep = [False] * (count + 1)
    keep[0] = keep[count] = True
    stack = [(0, count)]
    while stack:
        first, last = stack.pop()
        if last - first < 2:
            continue
        start = np.array(closed[first])
        end = np.array(closed[last])
        line = end - start
        length = float(np.hypot(*line))
        best_index, best_distance = -1, epsilon
        for index in range(first + 1, last):
            offset = np.array(closed[index]) - start
            if length == 0.0:
                distance = float(np.hypot(*offset))
            else:
                distance = abs(float(line[0] * offset[1] - line[1] * offset[0])) / length
            if distance > best_distance:
                best_index, best_distance = index, distance
        if best_index > 0:
            keep[best_index] = True
            stack.append((first, best_index))
            stack.append((best_index, last))
    return [closed[index] for index in range(count) if keep[index]]


def smooth(points, rounds=2):
    """Chaikin corner cutting on a closed polygon."""
    result = [(float(x), float(y)) for x, y in points]
    for _ in range(rounds):
        cut = []
        for index in range(len(result)):
            ax, ay = result[index]
            bx, by = result[(index + 1) % len(result)]
            cut.append((0.75 * ax + 0.25 * bx, 0.75 * ay + 0.25 * by))
            cut.append((0.25 * ax + 0.75 * bx, 0.25 * ay + 0.75 * by))
        result = cut
    return result


def path_data(mask, sigma=1.1, epsilon=0.6, min_area=12.0):
    """Traced outline of a mask as SVG path data."""
    soft = gaussian_filter(mask.astype(np.float32), sigma)
    parts = []
    for loop in boundary_loops(soft > 0.5):
        points = smooth(loop)
        if polygon_area(points) < min_area:
            continue
        points = rdp(points, epsilon)
        if len(points) < 3:
            continue
        coords = [f"{x:.1f} {y:.1f}" for x, y in points]
        parts.append("M" + "L".join(coords) + "Z")
    return "".join(parts)


def hexcolor(color):
    values = [int(round(min(max(float(channel), 0.0), 255.0))) for channel in color]
    return "#%02x%02x%02x" % tuple(values)


def write_svg(path, radius, side, samples, vignette, layers, faces):
    stops = "".join(
        f'    <stop offset="{position:.3f}" stop-color="{hexcolor(color)}"/>\n'
        for position, color in samples
    )
    defs = ""
    for name, light, dark in faces:
        defs += (
            f'    <linearGradient id="{name}" x1="0" y1="0" x2="0.65" y2="1">\n'
            f'      <stop offset="0" stop-color="{hexcolor(light)}"/>\n'
            f'      <stop offset="1" stop-color="{hexcolor(dark)}"/>\n'
            "    </linearGradient>\n"
        )
    overlay = ""
    if vignette > 0.02:
        overlay = (
            '  <radialGradient id="vignette" cx="0.5" cy="0.5" r="0.72">\n'
            '    <stop offset="0.5" stop-color="#000000" stop-opacity="0"/>\n'
            f'    <stop offset="1" stop-color="#000000" stop-opacity="{vignette:.3f}"/>\n'
            "  </radialGradient>\n"
        )
    body = (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{side}" height="{side}"'
        f' viewBox="0 0 {side} {side}">\n'
        "  <!-- Generated by scripts/make-icons.py from the master ZapExt logo. -->\n"
        f'  <defs>\n    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">\n{stops}    </linearGradient>\n{defs}{overlay}  </defs>\n'
        f'  <rect width="{side}" height="{side}" rx="{radius:.1f}" fill="url(#bg)"/>\n'
    )
    if vignette > 0.02:
        body += (
            f'  <rect width="{side}" height="{side}" rx="{radius:.1f}"'
            ' fill="url(#vignette)"/>\n'
        )
    for paint, data in layers:
        body += f'  <path fill="{paint}" fill-rule="evenodd" d="{data}"/>\n'
    body += "</svg>\n"
    path.write_text(body, encoding="utf-8")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", default=str(ROOT / "assets" / "zapext.png"))
    parser.add_argument("--vector-side", type=int, default=VECTOR_SIDE)
    args = parser.parse_args()

    rgb = load_source(args.source)
    x0, y0, x1, y1 = find_bounds(rgb)
    radius = find_radius(rgb, x0, y0, x1, y1)
    side = x1 - x0 + 1
    print(f"source: {args.source} ({rgb.shape[1]}x{rgb.shape[0]})")
    print(f"artwork: {side}px square at ({x0}, {y0}), corner radius {radius}px")

    scale = args.vector_side / float(side)
    radius_vector = radius * scale
    small = (
        Image.fromarray(rgb.astype(np.uint8))
        .crop((x0, y0, x1 + 1, y1 + 1))
        .resize((args.vector_side, args.vector_side), Image.LANCZOS)
    )
    small_rgb = np.asarray(small).astype(np.float32)
    coverage = coverage_mask(args.vector_side, args.vector_side, radius_vector)
    u = diagonal(small_rgb.shape)

    artwork = np.zeros(u.shape, dtype=bool)
    samples = []
    for _ in range(4):
        samples = background_samples(small_rgb, coverage, u, artwork)
        refined = artwork_core(small_rgb, samples, coverage, u)
        if np.array_equal(refined, artwork):
            break
        artwork = refined
    artwork = artwork_core(small_rgb, samples, coverage, u)
    predicted = curve_at(samples, u)
    print("background stops:", " ".join(hexcolor(color) for _, color in samples))
    print(f"artwork pixels: {int(artwork.sum())}")

    def layer_color(mask):
        if not mask.any():
            return np.array([0.0, 0.0, 0.0])
        return np.median(small_rgb[mask], axis=0)

    white, mint, shadow = artwork_layers(small_rgb, artwork, predicted)
    layers = []
    faces = []
    for name, mask, paint in (
        ("shadow", shadow, None),
        ("mint", mint, "url(#mint-face)"),
        ("white", white, "url(#white-face)"),
    ):
        if not mask.any():
            print(f"  {name}: empty")
            continue
        data = path_data(mask)
        if not data:
            print(f"  {name}: no path")
            continue
        if paint is None:
            layers.append((hexcolor(layer_color(mask)), data))
            print(f"  {name}: {int(mask.sum())} px, {len(data)} path chars, {hexcolor(layer_color(mask))}")
        else:
            light, dark = face_gradient(small_rgb[mask])
            faces.append((paint[len("url(#") : -1], light, dark))
            layers.append((paint, data))
            print(
                f"  {name}: {int(mask.sum())} px, {len(data)} path chars,"
                f" gradient {hexcolor(light)} to {hexcolor(dark)}"
            )

    vignette = vignette_opacity(small_rgb, samples, coverage, artwork, u)
    print(f"edge darkening overlay: {vignette:.3f}")

    svg_path = ROOT / "packaging" / "icons" / "zapfast.svg"
    write_svg(svg_path, radius_vector, args.vector_side, samples, vignette, layers, faces)
    print("wrote", svg_path.relative_to(ROOT), f"({svg_path.stat().st_size} bytes)")

    for target in (
        ROOT / "assets" / "zapext.png",
        ROOT / "packaging" / "macos" / "icon-1024.png",
    ):
        icon_rgba(rgb, x0, y0, x1, y1, radius, 1024, soften=True).save(
            target, optimize=True
        )
        print("wrote", target.relative_to(ROOT))

    frames = [icon_rgba(rgb, x0, y0, x1, y1, radius, size) for size in ICON_SIZES]
    ico = ROOT / "packaging" / "windows" / "zapfast.ico"
    frames[-1].save(ico, sizes=[(size, size) for size in ICON_SIZES])
    print("wrote", ico.relative_to(ROOT))
    return 0


if __name__ == "__main__":
    sys.exit(main())
