# Animation

This document describes the animations run by the reference firmware
([`src/main.rs`](../src/main.rs)) and how each one executes, frame by frame.

The panel runs in **landscape** orientation (320 × 240). The screen is split into
**three horizontal zones** along the 240 px vertical axis:

```
 y = 0   +-------------------------------------------+  \
         |  TOP fixed zone  -> "Hello CH32V003!"     |   } TOP_H = 50 px (fixed)
 y = 50  +-------------------------------------------+  /
         |                                           |  \
         |  MIDDLE scrolling zone -> color bars      |   } MID_H = 140 px (animated)
         |                                           |  /
 y = 190 +-------------------------------------------+  \
         |  BOTTOM fixed zone -> "ILI9341 + SPI DMA" |   } BOT_H = 50 px (fixed)
         |                       "PWM backlight ..." |  /
 y = 240 +-------------------------------------------+
```

Two animations run together, both confined to the **middle** zone or the
backlight — the top and bottom text zones are static:

1. **Scrolling color bars** — the middle band's pattern shifts to the right.
2. **Backlight brightness sweep** — the whole panel fades dark↔bright.

Because the scroll only repaints the middle band, the top/bottom text is drawn
**once** and never overwritten, so it **does not flicker**.

> **Why software scroll?** The ILI9341 hardware scroll moves along the panel's
> native 320-line axis, which is *horizontal* in landscape — it would slide the
> bars sideways with fixed columns on the left/right, and could not keep the
> top/bottom text fixed. Keeping the layout in landscape with fixed top/bottom
> text therefore uses a software repaint of the middle band. (See §4.)

---

## 1. Zones

```rust
const TOP_H: u16 = 50;                    // top fixed text band
const BOT_H: u16 = 50;                    // bottom fixed text band
const MID_Y: u16 = TOP_H;                 // scrolling band starts here
const MID_H: u16 = HEIGHT - TOP_H - BOT_H; // 240 - 50 - 50 = 140 px
```

---

## 2. Startup (drawn once)

Before the loop starts, the screen is cleared and the two fixed zones are
painted a single time by `draw_fixed`:

```rust
fn draw_fixed(display: &mut Ili9341) {
    display.fill_rect(0, 0, WIDTH, TOP_H, BLACK);            // top band background
    display.fill_rect(0, HEIGHT - BOT_H, WIDTH, BOT_H, BLACK); // bottom band background

    let title = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    Text::new("Hello CH32V003!", Point::new(8, 32), title).draw(display).unwrap();

    let subtitle = MonoTextStyle::new(&FONT_6X10, Rgb565::YELLOW);
    Text::new("ILI9341 + SPI DMA",   Point::new(8, HEIGHT as i32 - 30), subtitle).draw(display).unwrap();
    Text::new("PWM backlight sweep", Point::new(8, HEIGHT as i32 - 14), subtitle).draw(display).unwrap();
}
```

The text is drawn on a solid background here, so it reads cleanly and stays crisp
for the whole session.

---

## 3. Main loop

The loop is free-running (no timers/delays — each iteration is one frame, paced
by how long the SPI/DMA writes take):

```rust
display.fill_screen(BLACK);
draw_fixed(&mut display);   // top + bottom zones, drawn once

let mut offset: u16 = 0;    // bar scroll position, in pixels
let mut level: i32 = 0;     // backlight brightness, 0..=100 %
let mut step: i32 = 2;      // brightness direction/speed
loop {
    offset = (offset + 4) % WIDTH;   // advance the bars
    draw_bars(&mut display, offset); // repaint ONLY the middle band

    level += step;                   // advance brightness
    if level >= 100 { level = 100; step = -2; }
    else if level <= 0 { level = 0; step = 2; }
    backlight.set_duty(Channel::Ch2, bl_max * level as u32 / 100);
}
```

---

## 4. Scrolling color bars (middle zone)

### Pattern

Eight vertical rainbow bars, repeating every full screen width:

```rust
const BARS: [u16; 8] = [
    0xF800, // red
    0xFD00, // orange
    0xFFE0, // yellow
    0x07E0, // green
    0x07FF, // cyan
    0x001F, // blue
    0x4810, // indigo
    0xF81F, // violet
];
const BAR_W: u16 = WIDTH / 8;   // 320 / 8 = 40 px per bar
```

### How it moves

This is a **software scroll**. Each frame the scroll position advances by 4 px
and wraps at the screen edge:

```rust
offset = (offset + 4) % WIDTH;   // 0, 4, 8, ... 316, 0, ...
```

`draw_bars` repaints **only the middle band** (`MID_Y .. MID_Y + MID_H`) using
the shifted pattern. It walks across the screen in column runs, where each run is
the slice of one bar visible before the next color boundary:

```rust
fn draw_bars(display: &mut Ili9341, offset: u16) {
    let mut x = 0u16;
    while x < WIDTH {
        let pos = (x + offset) % WIDTH;                  // pattern coord at this column
        let band = (pos / BAR_W) as usize % BARS.len();  // which color
        let seg = (BAR_W - pos % BAR_W).min(WIDTH - x);  // run length to next boundary
        display.fill_rect(x, MID_Y, seg, MID_H, BARS[band]); // fill only the middle band
        x += seg;
    }
}
```

Because `offset` is rarely a multiple of `BAR_W`, a frame is usually drawn as
**nine runs**: a partial bar at the left edge, seven full 40-px bars, and a
partial bar at the right edge — all wrapping seamlessly.

### Why it is fast

Each run is a single **no-increment DMA fill** (`fill_rect` → `dma_fill`): the DMA
repeatedly copies one 16-bit color word from a fixed address into the SPI data
register, so a solid `seg × MID_H` block is streamed with no CPU loop and no RAM
frame buffer. See `doc/hardware.md` for the DMA details.

### Net effect

The bars in the middle band slide continuously to the right at 4 px/frame,
wrapping around so the motion never stops, while the text bands above and below
stay perfectly still.

### Why not hardware scroll?

The ILI9341 hardware scroll (`VSCRDEF 0x33` / `VSCRSAR 0x37`) shifts along the
panel's native 320-line axis. In landscape that axis is **horizontal**, so the
scroll would move the bars sideways and its fixed areas (TFA / BFA) would be
left/right columns — they could not hold the top/bottom text. Hardware scroll
with fixed *top/bottom* text would require switching the panel to portrait, so in
landscape the firmware uses the confined software repaint above.

---

## 5. Backlight brightness sweep

In parallel with the bars, the backlight PWM duty cycle ramps up and down to fade
the whole panel between off and full brightness:

```rust
level += step;                          // step = +2 going up, -2 coming down
if level >= 100 { level = 100; step = -2; }
else if level <= 0 { level = 0; step = 2; }
backlight.set_duty(Channel::Ch2, bl_max * level as u32 / 100);
```

- `level` is the brightness in percent, moving in ±2 % steps each frame.
- At 100 % it reverses to fade down; at 0 % it reverses to fade up — producing a
  continuous triangle-wave (breathing) effect.
- `bl_max * level / 100` maps the percentage onto the timer's full duty range, so
  the PWM on `PA1` (`TIM1_CH2`) tracks `level`.

Because brightness is controlled by the backlight (not by redrawing pixels), this
sweep affects all three zones equally and costs almost nothing.

---

## 6. Summary

| Zone / animation     | Region                  | Per-frame change          | Mechanism                          |
|----------------------|-------------------------|---------------------------|------------------------------------|
| Top text (fixed)     | `0 .. TOP_H`            | none (drawn once)         | `fill_rect` + text on solid bg     |
| Scrolling bars       | `MID_Y .. MID_Y+MID_H`  | `+4 px`, wrap at `WIDTH`  | Software repaint via no-incr. DMA  |
| Bottom text (fixed)  | `HEIGHT-BOT_H .. HEIGHT`| none (drawn once)         | `fill_rect` + text on solid bg     |
| Brightness sweep     | whole panel             | `±2 %`, bounce at 0/100   | PWM duty on `TIM1_CH2` (`PA1`)     |
