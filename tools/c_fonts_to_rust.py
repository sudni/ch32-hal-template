#!/usr/bin/env python3
"""Convert Tilen Majerle fonts.c (TM_Font7x10 / 11x18 / 16x26) to Rust static arrays."""

from __future__ import annotations

import re
import sys
from pathlib import Path


def parse_array(text: str, name: str) -> list[int]:
    pattern = rf"const uint16_t {name}\s*\[\]\s*=\s*\{{(.*?)\}};"
    m = re.search(pattern, text, re.DOTALL)
    if not m:
        raise SystemExit(f"array {name} not found")
    vals = re.findall(r"0x[0-9A-Fa-f]+", m.group(1))
    return [int(v, 16) for v in vals]


def emit_array(name: str, values: list[int], chunk: int = 10) -> str:
    lines = [f"pub static {name}: [u16; {len(values)}] = ["]
    for i in range(0, len(values), chunk):
        part = ", ".join(f"0x{v:04X}" for v in values[i : i + chunk])
        lines.append(f"    {part},")
    lines.append("];")
    return "\n".join(lines)


def main() -> None:
    src = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(
        r"D:\Warehouse\C_dev\CH32V003-SPI-DMA---ILI9341\drivers\fonts.c"
    )
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(
        r"D:\Warehouse\rust\ch32v003_ili9341_bareMetal_rust\src\font\data.rs"
    )

    text = src.read_text(encoding="utf-8", errors="replace")
    f7 = parse_array(text, "TM_Font7x10")
    f11 = parse_array(text, "TM_Font11x18")
    f16 = parse_array(text, "TM_Font16x26")

    body = """//! Bitmap font data (Tilen Majerle / GPL-3), converted from `fonts.c`.

"""
    body += emit_array("DATA_7X10", f7) + "\n\n"
    body += emit_array("DATA_11X18", f11) + "\n\n"
    body += emit_array("DATA_16X26", f16) + "\n"

    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(body, encoding="utf-8")
    print(f"wrote {out} ({len(f7)}, {len(f11)}, {len(f16)} values)")


if __name__ == "__main__":
    main()
