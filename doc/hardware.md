# Hardware Connection & Configuration

This document describes how to wire a **ILI9341 240×320 TFT display** to a
**CH32V003** microcontroller and the firmware configuration used to drive it
(SPI + DMA, with PWM-controlled backlight).

> Pin assignments below match the reference firmware in
> [`src/main.rs`](../src/main.rs).

---

## 1. Wiring

The display is driven over **SPI1** with **DMA** (mandatory on this devkit). The
backlight is driven by a PWM signal from **Timer 1, Channel 2**.

> **Devkit pin constraint:** this board only breaks out `PA1`, `PA2`,
> `PC0`–`PC7`, and `PD0`–`PD3`. The pin map below is chosen to fit those pins.
> On the CH32V003 the **SPI1 pins are fixed by hardware** — `SCK = PC5`,
> `MOSI = PC6`, `MISO = PC7` — so they cannot be remapped to other GPIOs.

| ILI9341 pin | Function                      | CH32V003 pin | Mode                    |
|-------------|-------------------------------|--------------|-------------------------|
| `VCC`       | Power (3.3 V)                 | `3V3`        | —                       |
| `GND`       | Ground                        | `GND`        | —                       |
| `CS`        | Chip select                   | `PC0`        | Push-pull output        |
| `RESET`     | Hardware reset                | `PC2`        | Push-pull output        |
| `DC` / `RS` | Data/Command select           | `PC1`        | Push-pull output        |
| `SDI`/`MOSI`| SPI data → display (fixed)    | `PC6`        | Alternate push-pull     |
| `SCK`       | SPI clock (fixed)             | `PC5`        | Alternate push-pull     |
| `LED`/`BLK` | Backlight (PWM)               | `PA1`        | Alternate (TIM1_CH2)    |
| `SDO`/`MISO`| SPI data ← display (optional) | `PC7`        | Floating input          |

> `MISO` (`PC7`) is only required if you read data back from the panel. For a
> write-only display it can be left unconnected, but the pin is reserved by SPI1.
>
> **Free pins** remaining for other use: `PA2`, `PC3`, `PC4`, `PD0`–`PD3`.

### Connection diagram

```
   CH32V003                         ILI9341 (SPI)
 ┌───────────┐                    ┌──────────────┐
 │      3V3  ├────────────────────┤ VCC          │
 │      GND  ├────────────────────┤ GND          │
 │      PC0  ├────────────────────┤ CS           │
 │      PC2  ├────────────────────┤ RESET        │
 │      PC1  ├────────────────────┤ DC / RS      │
 │      PC6  ├────────────────────┤ SDI / MOSI   │  (SPI1_MOSI, fixed)
 │      PC5  ├────────────────────┤ SCK          │  (SPI1_SCK,  fixed)
 │      PA1  ├────────────────────┤ LED / BLK    │  (TIM1_CH2)
 │      PC7  ├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┤ SDO / MISO   │  (SPI1_MISO, optional)
 └───────────┘                    └──────────────┘
```

---

## 2. Power

- Supply the display from a **3.3 V** rail. The CH32V003 is a 3.3 V part; do
  **not** use 5 V on the logic pins.
- The backlight LED can draw tens of mA. If your module does not include a
  series resistor on `LED`, add one (or drive it through the PWM pin as shown)
  to avoid over-driving the GPIO.
- Add a **100 nF decoupling capacitor** close to the display `VCC`/`GND` pins.

---

## 3. SPI configuration

| Parameter   | Value                                   |
|-------------|-----------------------------------------|
| Peripheral  | `SPI1`                                  |
| Role        | Master                                  |
| Clock speed | `24 MHz` (HCLK/2 at 48 MHz)             |
| Data size   | `8 bits`                                |
| SPI mode    | Mode 0 (CPOL = 0, CPHA = 0)             |
| Bit order   | MSB first                               |
| Direction   | TX only (write-only to the panel)       |
| Remap       | `0` (`SCK = PC5`, `MOSI = PC6`)         |
| Fixed pins  | `SCK = PC5`, `MOSI = PC6`, `MISO = PC7` |

The SPI peripheral is created **blocking** (used directly for commands and
per-pixel writes). Bulk fills are handled separately by driving `DMA1_CH3`
directly (see below).

```rust
// Run the core at 48 MHz (HSI 24 MHz -> PLL x2) so HCLK = 48 MHz.
let mut config = hal::Config::default();
config.rcc = hal::rcc::Config::SYSCLK_FREQ_48MHZ_HSI;
let p = hal::init(config);

let mut spi_config = hal::spi::Config::default(); // Mode 0, MSB first
spi_config.frequency = Hertz::mhz(24);            // HCLK/2 = 24 MHz
// SPI1 TX-only; remap 0 -> SCK = PC5, MOSI = PC6.
let spi = Spi::new_blocking_txonly::<0>(p.SPI1, p.PC5, p.PC6, spi_config);
```

> **System clock & SPI speed:** SPI1's baud clock is **HCLK**, and the minimum
> divider is **/2**, so the fastest SPI clock is HCLK/2. The HAL's *default*
> config runs HCLK at only **8 MHz** (HSI 24 MHz ÷3), which would cap SPI at
> 4 MHz. To reach 24 MHz SPI the firmware raises the core to **48 MHz** with the
> `SYSCLK_FREQ_48MHZ_HSI` preset (HSI 24 MHz → PLL ×2 → HCLK 48 MHz); then
> `frequency = 24 MHz` selects the /2 divider. Requesting a value above HCLK/2
> just clamps to HCLK/2. If you see artifacts on long/poor wiring, drop the
> requested frequency (e.g. 12 MHz → /4) or shorten the leads.

### DMA fills (memory-to-peripheral, no increment)

Solid fills (clearing the screen, color bars, rectangles) are the bulk of the
SPI traffic, so they go out over DMA. The CH32V003's DMA fully supports
**memory-to-peripheral with no memory increment**, which is ideal here: the DMA
reads a *single* 16-bit color word from one fixed address and writes it to the
SPI data register `count` times. No RAM frame buffer is needed and the CPU is
idle during the transfer.

The DMA channel for `SPI1_TX` is **fixed by hardware to DMA1 Channel 3** on the
CH32V003 (`SPI1_RX` is Channel 2). It cannot be reassigned.

To fill, the driver:

1. Switches SPI to **16-bit frames** (`DFF = 1`) so one DMA word = one pixel.
   MSB-first framing sends the high byte then low byte, matching RGB565.
2. Enables `TXDMAEN` and starts a no-increment transfer of the color word.
3. Waits for completion, then waits for `BSY` to clear and restores 8-bit
   framing for the next command / text write.

```rust
// Stream `count` copies of one 16-bit color, no memory increment, no buffer.
let datar = hal::pac::SPI1.datar().as_ptr() as *mut u16;
unsafe {
    Transfer::new_write_repeated::<u16>(
        dma.reborrow(),           // Peri<DMA1_CH3>
        Default::default(),       // request
        &color,                   // single source word (fixed address)
        count,                    // up to 65535 per transfer
        datar,
        TransferOptions::default(),
    )
    .blocking_wait();
}
```

> A single DMA transfer is limited to 65535 items, so a full-screen fill
> (320 × 240 = 76 800 px) is split into two transfers. `blocking_wait()` polls
> to completion, so no async executor is required.

---

## 4. Reset sequence

After power-up, the display requires a hardware reset pulse on `RESET` (`PC2`):

```rust
rst.set_low();
delay.delay_ms(20);   // hold reset low ≥ 10 ms
rst.set_high();
delay.delay_ms(120);  // wait ≥ 120 ms before sending commands
```

---

## 5. Backlight (PWM)

The backlight brightness is controlled via PWM on `PA1` using **Timer 1,
Channel 2** (`TIM1_CH2` is available on `PA1`):

| Parameter      | Value          |
|----------------|----------------|
| Timer          | `TIM1`         |
| Channel        | `CH2`          |
| Pin            | `PA1`          |
| PWM frequency  | `1 kHz`        |
| Duty range     | `0 – 100` (%)  |

```rust
let bl_pin = PwmPin::new_ch2::<0>(p.PA1);            // PA1 = TIM1_CH2, remap 0
let mut backlight = SimplePwm::new(
    p.TIM1, None, Some(bl_pin), None, None,
    Hertz::khz(1), CountingMode::default(),
);
let bl_max = backlight.get_max_duty();
backlight.enable(Channel::Ch2);
backlight.set_duty(Channel::Ch2, bl_max * 50 / 100);  // 50 % brightness
```

Brightness is `bl_max * percent / 100`. The demo sweeps it 0–100 % continuously.

- If your module's backlight is **active-low** (driven via an inverting
  transistor), the fade runs backwards — call
  `backlight.set_polarity(Channel::Ch2, OutputPolarity::ActiveLow)`.
- For a **fixed-on** backlight instead of PWM, tie `LED`/`BLK` to `3V3`
  (through a current-limiting resistor) and drive `PA1` as a plain output, or
  just leave it at 100 % duty.

---

## 6. Display configuration

| Parameter    | Value                       |
|--------------|-----------------------------|
| Controller   | ILI9341                     |
| Resolution   | 320 × 240 (landscape)       |
| Orientation  | Landscape (`MADCTL = 0x28`) |
| Color format | RGB565 (16-bit)             |

The firmware uses a small **custom ILI9341 driver** (in `src/main.rs`) instead
of a heavyweight crate, to fit the CH32V003's 16 KB flash / 2 KB RAM. It sends
init commands and per-pixel writes blocking, and streams bulk fills over DMA.

Orientation is set by the `MADCTL` (`0x36`) register. Width/height are swapped
to match:

| Orientation       | `MADCTL` | Logical size |
|-------------------|----------|--------------|
| Portrait          | `0x48`   | 240 × 320    |
| **Landscape**     | `0x28`   | **320 × 240**|
| Portrait flipped  | `0x88`   | 240 × 320    |
| Landscape flipped | `0xE8`   | 320 × 240    |

If colors look swapped (red ↔ blue), clear the `BGR` bit (`0x08`) in the value.

### Text & graphics

The driver implements `embedded-graphics` `DrawTarget`, so you can draw text and
shapes. The firmware draws text into two fixed bands (see §7), filling a solid
background first and then the glyphs on top:

```rust
display.fill_rect(0, 0, WIDTH, TOP_H, BLACK);              // solid band background
let style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
Text::new("Hello CH32V003!", Point::new(8, 32), style)
    .draw(&mut display)
    .unwrap();
```

For a **transparent** overlay (glyph pixels only, background shows through),
just skip the `fill_rect`. For an **opaque** background per character, set
`style.background_color = Some(Rgb565::BLACK);` on the style instead.

---

## 7. Demo animation

The reference firmware runs a self-contained demo. The screen is split into
**three horizontal zones** along the 240 px vertical axis:

| Zone   | Rows (`y`)              | Height | Content                                  |
|--------|-------------------------|--------|------------------------------------------|
| Top    | `0 .. TOP_H`            | 50 px  | Fixed text: `"Hello CH32V003!"`          |
| Middle | `TOP_H .. TOP_H+MID_H`  | 140 px | Scrolling color bars (animated)          |
| Bottom | `HEIGHT-BOT_H .. HEIGHT`| 50 px  | Fixed text: `"ILI9341 + SPI DMA"` + `"PWM backlight sweep"` |

Two effects run together:

1. **Scrolling color bars** (middle zone only): eight vertical rainbow bars
   (red, orange, yellow, green, cyan, blue, indigo, violet — 40 px each) slide
   horizontally and wrap around.
2. **PWM brightness sweep:** the backlight duty fades 0 → 100 → 0 % in parallel,
   affecting all three zones equally.

The full animation walkthrough lives in [`animation.md`](./animation.md).

### Fixed zones drawn once, middle band scrolled

The top/bottom text bands are painted **once** by `draw_fixed` before the loop.
The loop then repaints **only the middle band** each frame, so the text is never
overwritten and **does not flicker**:

```rust
draw_fixed(&mut display);            // top + bottom text, drawn once
let mut offset = 0u16;
loop {
    offset = (offset + 4) % WIDTH;   // 4 px/frame
    draw_bars(&mut display, offset); // repaints MID_Y..MID_Y+MID_H only
    // ... advance PWM brightness ...
}
```

`draw_bars` splits the middle band into a handful of solid runs (one per visible
bar segment, accounting for wrap-around) and fills each — `seg × MID_H` — with a
single no-increment DMA transfer, so the redraw is cheap and uses no frame buffer.

### Why not hardware scrolling?

The ILI9341 *does* have a hardware scroll (`VSCRDEF 0x33` / `VSCRSAR 0x37`) with
its own top/bottom fixed areas (TFA / BFA). But it scrolls along the native
320-line axis, which in this **landscape** orientation maps to the **horizontal**
direction — whereas these three zones are stacked **vertically**. The axes don't
line up, so the firmware uses the confined software repaint above instead. (To
use hardware scroll with fixed top/bottom text you'd switch the panel to
portrait, where that axis is vertical.)

---

## 8. Toolchain & build configuration

This project targets the CH32V003 (QingKe RV32EC core) and builds with a custom
target spec.

| Item             | Value                                              |
|------------------|----------------------------------------------------|
| Toolchain        | `nightly` (with `rust-src`)                        |
| Target           | `riscv32ec-unknown-none-elf` (custom JSON spec)    |
| ABI              | `ilp32e`                                           |
| ISA features     | `+e, +c, +forced-atomics`                          |
| `build-std`      | `core`                                             |
| HAL feature      | `ch32v003f4p6` (set to your exact variant)         |
| Flash runner     | `wlink -v flash --enable-sdi-print --watch-serial` |

Build and flash:

```bash
cargo build --release
cargo run --release   # builds + flashes via wlink (WCH-LinkE)
```

> The warning `skipping unavailable component rust-std for target
> riscv32ec-unknown-none-elf` is expected — the standard library `core` is
> compiled from source via `build-std`.

---

## 9. Notes & adjustments

- **MCU variant:** the reference build uses `ch32v003f4p6`. If your board uses a
  different package (`ch32v003a4m6`, `ch32v003f4u6`, `ch32v003j4m6`), update the
  `ch32-hal` feature in `Cargo.toml` and confirm the pins above are exposed on
  that package.
- **Pin availability:** the CH32V003 has a small pin count. Verify each pin in
  the table is broken out on your specific package/board before wiring.
- **Clock speed:** the firmware raises the core to **48 MHz** (HSI 24 MHz → PLL
  ×2) via `SYSCLK_FREQ_48MHZ_HSI`, giving HCLK = 48 MHz and a **24 MHz** SPI clock
  (HCLK/2, the bus maximum). The HAL default would otherwise run HCLK at 8 MHz.
  Reduce the SPI `frequency` (e.g. 12 MHz → /4) if you see artifacts on long wiring.
- **Flash headroom:** with `embedded-graphics` and the `FONT_10X20` + `FONT_6X10`
  fonts, the release build is ~14 KB of the 16 KB flash. Fonts dominate that —
  drop `FONT_10X20` if you need more room.
