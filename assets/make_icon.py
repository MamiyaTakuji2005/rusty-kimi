"""Draw the DvaDva mark: 2 squared, the arithmetic the project is named for.

Produces the two forms the binaries need, from one description:

  dvadva.ico     the Windows resource each crate's `build.rs` embeds — what
                 Explorer, the taskbar and Alt+Tab show for the .exe file
                 itself. Shared by inkvizitor, dvadva-agent and dvadva-tui:
                 one project, one mark.
  icon-64.rgba   raw RGBA, `include_bytes!`d by inkvizitor's main.rs for the
                 *window* icon at run time (eframe wants pixels, and decoding
                 a PNG would mean pulling the `image` crate in for 16 KB of
                 them). The other two are console binaries with no window.

Run it when the mark changes, from anywhere:  python assets/make_icon.py

Every size is drawn at 8x and filtered down rather than rendered small: a
16px tile straight out of a hinted font loses the exponent entirely.
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

# Everything is written beside this file, whatever the working directory is:
# the build scripts look for the mark at a fixed path, and a run from the
# repo root should not scatter three files across it.
HERE = Path(__file__).parent

# The app's own palette (theme.rs): the Kimi accent cyan, on the darkest of
# its slates. A bright tile rather than a dark one — the taskbar it has to
# stand out against is usually dark itself.
CYAN = (0x67, 0xE8, 0xF9, 0xFF)
SLATE = (0x0B, 0x12, 0x20, 0xFF)

FONT = r"C:\Windows\Fonts\seguibl.ttf"  # Segoe UI Black: heavy enough to read small
SIZES = [16, 24, 32, 48, 64, 128, 256]
SS = 8  # supersampling factor


def tile(size):
    """One square icon at `size`, drawn at `size * SS` and filtered down."""
    s = size * SS
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    # A rounded square, not a circle: it sits in a row of square app icons.
    draw.rounded_rectangle((0, 0, s - 1, s - 1), radius=s * 0.22, fill=CYAN)

    base = ImageFont.truetype(FONT, int(s * 0.55))
    expo = ImageFont.truetype(FONT, int(s * 0.31))

    # Lay the two out as one word and centre *that*, on both axes. Centring
    # them separately would leave the pair looking pushed to one side, and
    # centring on the base alone ignores the exponent standing above it,
    # which drops the whole mark toward the bottom edge.
    bb = draw.textbbox((0, 0), "2", font=base)
    eb = draw.textbbox((0, 0), "2", font=expo)
    bw, bh = bb[2] - bb[0], bb[3] - bb[1]
    ew, eh = eb[2] - eb[0], eb[3] - eb[1]

    gap = s * 0.02
    rise = eh * 0.40  # how far the exponent's top clears the base's
    left = (s - (bw + gap + ew)) / 2
    top = (s - (bh + rise)) / 2 + rise

    draw.text((left - bb[0], top - bb[1]), "2", font=base, fill=SLATE)
    draw.text(
        (left + bw + gap - eb[0], top - rise - eb[1]),
        "2",
        font=expo,
        fill=SLATE,
    )

    return img.resize((size, size), Image.LANCZOS)


def main():
    tiles = [tile(size) for size in SIZES]

    # Pillow matches an appended image to each requested size and resizes
    # only the ones it has no match for -- but it silently *drops* any size
    # larger than the image it was called on, so the call has to be made on
    # the biggest tile or the .ico quietly comes out 16px and nothing else.
    tiles[-1].save(
        HERE / "dvadva.ico",
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=tiles[:-1],
    )
    print("wrote dvadva.ico", SIZES)

    icon = tiles[SIZES.index(64)]
    (HERE / "icon-64.rgba").write_bytes(icon.tobytes())
    print("wrote icon-64.rgba", len(icon.tobytes()), "bytes")

    # A contact sheet, for looking at the small sizes the way they will
    # actually appear — and again magnified, to see what got lost.
    sheet = Image.new("RGBA", (600, 300), (0x1E, 0x1E, 0x1E, 0xFF))
    x = 10
    for size, img in zip(SIZES, tiles):
        sheet.paste(img, (x, 10), img)
        big = img.resize((96, 96), Image.NEAREST)
        sheet.paste(big, (x, 150), big)
        x += max(size, 96) + 12
    sheet.save(HERE / "preview.png")
    print("wrote preview.png")


if __name__ == "__main__":
    main()
