#!/usr/bin/env python3
"""Add Herdr marks to Hack Nerd Font without merging layout/metrics tables."""

import argparse
import hashlib
from pathlib import Path

from fontTools.pens.recordingPen import DecomposingRecordingPen
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont

ROOT = Path(__file__).resolve().parents[1]
STYLES = {
    "Regular": "Regular",
    "Bold": "Bold",
    "Italic": "Italic",
    "BoldItalic": "Bold Italic",
}
FAMILY = "Hack Herdr Nerd Font"


def build(base_path, icons_path, output_path, style):
    font = TTFont(base_path, recalcTimestamp=False)
    icons = TTFont(icons_path, recalcTimestamp=False)
    base_cmap = dict(font.getBestCmap())
    icon_cmap = icons.getBestCmap()
    conflicts = set(base_cmap) & set(icon_cmap)
    if conflicts:
        raise ValueError(
            f"Refusing to replace existing characters: {sorted(conflicts)}"
        )
    base_metrics = dict(font["hmtx"].metrics)
    base_line = (
        font["hhea"].ascent,
        font["hhea"].descent,
        font["hhea"].lineGap,
    )
    order = list(font.getGlyphOrder())
    scale = font["head"].unitsPerEm / icons["head"].unitsPerEm
    advance = font["hmtx"][base_cmap[32]][0]
    source = icons.getGlyphSet()
    for cp, source_name in icon_cmap.items():
        name = f"herdr_{source_name}"
        if name in order:
            raise ValueError(f"Conflicting glyph name: {name}")
        recording = DecomposingRecordingPen(source)
        source[source_name].draw(recording)
        pen = TTGlyphPen(None)
        recording.replay(TransformPen(pen, (scale, 0, 0, scale, 0, 0)))
        glyph = pen.glyph()
        font["glyf"][name] = glyph
        glyph.recalcBounds(font["glyf"])
        # Max artwork overhangs one cell, as in the original icon-only face.
        font["hmtx"][name] = (advance, glyph.xMin)
        order.append(name)
        for table in font["cmap"].tables:
            if table.isUnicode() and table.format in (4, 12):
                table.cmap[cp] = name

    font.setGlyphOrder(order)
    names = font["name"]
    base_version = names.getDebugName(5)
    copyright_notice = names.getDebugName(0)
    license_notice = names.getDebugName(13)
    ps_name = f"HackHerdrNF-{style}"
    values = {
        0: (
            copyright_notice
            + "; Herdr icon marks: see bundled third-party notices."
        ),
        1: FAMILY,
        2: STYLES[style],
        3: f"1.000;{ps_name}",
        4: f"{FAMILY} {STYLES[style]}",
        5: (
            f"Version 1.000; based on {base_version}; "
            f"icons: {icons['name'].getDebugName(5)}"
        ),
        6: ps_name,
        13: (
            license_notice
            + "\n\nHerdr glyphs and Nerd Fonts components retain their "
            "original licenses. See the bundled licenses directory and "
            "NOTICE.md; vendor trademarks remain with their owners."
        ),
        16: FAMILY,
        17: STYLES[style],
    }
    for name_id, value in values.items():
        names.removeNames(nameID=name_id)
        names.setName(value, name_id, 3, 1, 0x409)
        names.setName(value, name_id, 1, 0, 0)

    output_path.parent.mkdir(parents=True, exist_ok=True)
    font.save(output_path)
    checked = TTFont(output_path, recalcTimestamp=False)
    cmap = checked.getBestCmap()
    if any(cmap.get(cp) != name for cp, name in base_cmap.items()):
        raise ValueError("Original character mappings changed")
    if any(
        checked["hmtx"][name] != metrics
        for name, metrics in base_metrics.items()
    ):
        raise ValueError("Original glyph metrics changed")
    if base_line != (
        checked["hhea"].ascent,
        checked["hhea"].descent,
        checked["hhea"].lineGap,
    ):
        raise ValueError("Original line metrics changed")
    if not set(icon_cmap) <= set(cmap):
        raise ValueError("An icon is missing from the output cmap")
    print(
        f"{output_path.name}: preserved {len(base_cmap)} mappings; "
        f"added {len(icon_cmap)} Herdr glyphs"
    )


def write_checksums(output_dir):
    lines = []
    for style in STYLES:
        path = output_dir / f"HackHerdrNerdFont-{style}.ttf"
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        lines.append(f"{digest}  {path.name}")
    (output_dir / "SHA256SUMS").write_text(
        "\n".join(lines) + "\n", encoding="utf-8"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base-dir",
        type=Path,
        required=True,
        help="Directory containing the four HackNerdFont-*.ttf files",
    )
    parser.add_argument(
        "--icons",
        type=Path,
        default=ROOT / "assets/fonts/HerdrAgentIconsMax-Regular.ttf",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=ROOT / "dist/hack-herdr",
    )
    args = parser.parse_args()
    for style in STYLES:
        build(
            args.base_dir / f"HackNerdFont-{style}.ttf",
            args.icons,
            args.output_dir / f"HackHerdrNerdFont-{style}.ttf",
            style,
        )
    write_checksums(args.output_dir)


if __name__ == "__main__":
    main()
