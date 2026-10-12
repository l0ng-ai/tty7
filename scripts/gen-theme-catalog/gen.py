#!/usr/bin/env python3
"""Generate src/ui/presets/catalog.rs from iTerm2-Color-Schemes.

    git clone --depth 1 https://github.com/mbadolato/iTerm2-Color-Schemes /tmp/schemes
    python3 -I scripts/gen-theme-catalog/gen.py /tmp/schemes > src/ui/presets/catalog.rs

Reads the `ghostty/` folder (one plain `key = value` file per scheme), keeps the
dark ones, and skips any the hand-tuned built-ins already cover or whose
foreground fails the same contrast floor the built-in tests hold every theme to.
"""

import json
import pathlib
import re
import subprocess
import sys
import unicodedata

# Names the hand-written BUILTINS in presets.rs already ship, normalised.
HAND_TUNED = {
    "dark", "dracula", "harbor", "onedarkpro", "onedark", "rosepine",
    "catppuccinmocha", "gruvboxdark", "nord", "tokyonight",
}
# Ids the built-ins (light ones included) already use.
TAKEN_IDS = {
    "light", "one_light", "catppuccin_latte", "rose_pine_dawn", "dark",
    "dracula", "harbor", "one_dark_pro", "rose_pine", "catppuccin_mocha",
    "gruvbox_dark", "nord", "tokyo_night",
}
# Schemes the UI derives an unreadable chrome from: hues that collapse into one
# graph lane, a muted label indistinguishable from the text, a wash that hides
# its glyph. Found by running the presets/element tests against each scheme on
# its own; rerun that when bumping the source.
EXCLUDE = {
    "acid_lime", "afterglow", "agave_dusk", "atelier_sulphurpool",
    "atlas_ragnarok", "aura", "aura_dark", "barrel_cactus",
    "belafonte_night", "black_metal_gorgoroth", "black_metal_marduk",
    "black_metal_nile", "blue_dolphin", "caliche", "canyon_shadow",
    "charcoal", "cholla", "ciapre", "copper_canyon", "crt_afterglow",
    "desert_morning", "desert_thunder", "django_smooth", "ectoplasm",
    "eyesclosed", "fairyfloss", "forge_ember", "grass", "graveyard_shift",
    "great_pumpkin", "hax0r_blue", "hax0r_gr33n", "isocon_dark",
    "iterm2_solarized_dark", "jute_espresso", "laser", "later_this_evening",
    "mellifluous", "mesa_blue", "mesila_one", "miasma", "monsoon_mesa",
    "n0tch2k", "nocturnal_winter", "nvim_dark", "ocean", "ocotillo",
    "ollie", "owl", "painted_desert", "patina", "petrified", "pueblo_night",
    "pumpkin_spice", "red_rock", "retro", "scabos_sand", "seoulbones_dark",
    "sequoia_monochrome_dark", "sequoia_moonlight_dark",
    "sequoia_retro_dark", "seti", "squirrelsong_dark", "the_fog",
    "toy_chest", "turquoise_trail", "twilight", "unikitty", "x_berlin",
    "x_oslo",
}
FG_FLOOR = 4.5  # The tests ask 4.0 of fg/bg; leave headroom for the tab row.


def lum(c):
    def ch(v):
        s = v / 255
        return s / 12.92 if s <= 0.03928 else ((s + 0.055) / 1.055) ** 2.4
    return 0.2126 * ch(c >> 16 & 255) + 0.7152 * ch(c >> 8 & 255) + 0.0722 * ch(c & 255)


def contrast(a, b):
    hi, lo = sorted((lum(a), lum(b)), reverse=True)
    return (hi + 0.05) / (lo + 0.05)


def norm(name):
    s = unicodedata.normalize("NFKD", name).encode("ascii", "ignore").decode()
    return re.sub(r"[^a-z0-9]", "", s.lower())


def ident(name):
    s = unicodedata.normalize("NFKD", name).encode("ascii", "ignore").decode()
    s = re.sub(r"\+", " plus ", s)
    return re.sub(r"[^a-z0-9]+", "_", s.lower()).strip("_")


def parse(path):
    palette, kv = {}, {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" not in line or line.lstrip().startswith("#"):
            continue
        k, v = (p.strip() for p in line.split("=", 1))
        if k == "palette":
            i, c = v.split("=", 1)
            palette[int(i)] = int(c.strip().lstrip("#"), 16)
        else:
            kv[k] = v
    if len(palette) < 16 or "background" not in kv or "foreground" not in kv:
        return None
    hexv = lambda s: int(s.lstrip("#"), 16)
    cursor = kv.get("cursor-color")
    return {
        "bg": hexv(kv["background"]),
        "fg": hexv(kv["foreground"]),
        "cursor": hexv(cursor) if cursor and re.fullmatch(r"#?[0-9a-fA-F]{6}", cursor) else None,
        "ansi": [palette[i] for i in range(16)],
    }


def pick_accent(t):
    """The scheme's blue, unless it sinks into the background or sits so close
    to the text that a switch knob (painted in the foreground) vanishes on it;
    then the first of its other hues that clears both."""
    bg, fg, ansi = t["bg"], t["fg"], t["ansi"]
    candidates = [ansi[i] for i in (4, 12, 6, 14, 5, 13, 2, 10, 3, 11)]
    for c in candidates:
        if contrast(bg, c) >= 3.0 and contrast(fg, c) >= 1.6:
            return c
    return max(candidates, key=lambda c: min(contrast(bg, c) / 3.0, contrast(fg, c) / 1.6))


def main():
    root = pathlib.Path(sys.argv[1])
    rev = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--short", "HEAD"],
        capture_output=True, text=True,
    ).stdout.strip()
    out, seen_ids, seen_names = [], set(TAKEN_IDS), set(HAND_TUNED)
    for path in sorted((root / "ghostty").iterdir(), key=lambda p: p.name.lower()):
        name = path.name
        t = parse(path)
        if t is None or lum(t["bg"]) >= 0.5:
            continue
        if norm(name) in seen_names or contrast(t["bg"], t["fg"]) < FG_FLOOR:
            continue
        tid = ident(name)
        if not tid or tid in seen_ids or tid in EXCLUDE:
            continue
        seen_ids.add(tid)
        seen_names.add(norm(name))
        accent = pick_accent(t)
        caret = f"Some(0x{t['cursor']:06x})" if t["cursor"] is not None else "None"
        ansi = ", ".join(f"0x{c:06x}" for c in t["ansi"])
        out.append(
            f'    spec("{tid}", {json.dumps(name, ensure_ascii=False)}, 0x{t["bg"]:06x}, 0x{t["fg"]:06x}, '
            f"0x{accent:06x}, {caret}, [{ansi}]),"
        )
    print(f"""//! Generated by `scripts/gen-theme-catalog/gen.py` - do not edit by hand.
//!
//! The dark schemes from iTerm2-Color-Schemes ({rev}),
//! <https://github.com/mbadolato/iTerm2-Color-Schemes>, MIT License,
//! Copyright (c) 2011 to Present Mark Badolato. The ones `BUILTINS` already
//! hand-tunes are left out.

use super::BuiltinSpec;

const fn spec(
    id: &'static str,
    name: &'static str,
    background: u32,
    foreground: u32,
    accent: u32,
    caret: Option<u32>,
    ansi: [u32; 16],
) -> BuiltinSpec {{
    let mut ansi16 = [(0u8, 0u8, 0u8); 16];
    let mut i = 0;
    while i < 16 {{
        ansi16[i] = ((ansi[i] >> 16) as u8, (ansi[i] >> 8) as u8, ansi[i] as u8);
        i += 1;
    }}
    BuiltinSpec {{
        id,
        name,
        background,
        foreground,
        accent,
        caret,
        ansi16,
    }}
}}

#[rustfmt::skip]
pub(super) static CATALOG: &[BuiltinSpec] = &[
{chr(10).join(out)}
];""")
    print(f"{len(out)} themes", file=sys.stderr)


if __name__ == "__main__":
    main()
