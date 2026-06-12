#![no_std]
#![no_main]

//! ILI9341 (320x240, landscape) over SPI1 with DMA on a CH32V003 devkit.
//!
//! Wiring (see `doc/hardware.md`):
//!   SCK  = PC5   (SPI1, fixed)
//!   MOSI = PC6   (SPI1, fixed)
//!   CS   = PC0
//!   DC   = PC1
//!   RST  = PC2
//!   BLK  = PA1   (backlight, TIM1_CH2 PWM)
//!
//! Bulk fills are streamed with DMA (SPI1_TX = DMA1_CH3) in
//! memory-to-peripheral, *no-increment* mode: the DMA reads a single 16-bit
//! color word from one fixed address and writes it to the SPI data register
//! `count` times. This needs no RAM frame buffer and is the fastest way to
//! fill the panel.
//!
//! The screen is laid out as three horizontal zones (see `doc/animation.md`):
//! a fixed top text band, a scrolling color-bar band in the middle, and a fixed
//! bottom text band. In landscape the ILI9341 hardware scroll would move
//! horizontally (fixed columns left/right), so it can't keep the top/bottom text
//! fixed; the middle band is therefore scrolled in software, repainting only that
//! band each frame so the text never moves.

use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use hal::delay::Delay;
use hal::dma::{Transfer, TransferOptions};
use hal::gpio::{Level, Output};
use hal::mode::Blocking;
use hal::spi::Spi;
use hal::time::Hertz;
use hal::timer::low_level::CountingMode;
use hal::timer::simple_pwm::{PwmPin, SimplePwm};
use hal::timer::Channel;
use hal::{peripherals, Peri};
use {ch32_hal as hal, panic_halt as _};

// Landscape orientation: 320 wide x 240 tall.
const WIDTH: u16 = 320;
const HEIGHT: u16 = 240;

// RGB565 colors.
const BLACK: u16 = 0x0000;

struct Ili9341 {
    spi: Spi<'static, peripherals::SPI1, Blocking>,
    dc: Output<'static>,
    dma: Peri<'static, peripherals::DMA1_CH3>,
}

impl Ili9341 {
    fn new(
        spi: Spi<'static, peripherals::SPI1, Blocking>,
        dc: Output<'static>,
        dma: Peri<'static, peripherals::DMA1_CH3>,
    ) -> Self {
        Self { spi, dc, dma }
    }

    /// Send a command byte followed by optional argument bytes.
    fn cmd(&mut self, cmd: u8, args: &[u8]) {
        self.dc.set_low();
        self.spi.blocking_write(&[cmd]).unwrap();
        if !args.is_empty() {
            self.dc.set_high();
            self.spi.blocking_write(args).unwrap();
        }
    }

    fn init(&mut self, delay: &mut Delay) {
        self.cmd(0x01, &[]); // SWRESET
        delay.delay_ms(150);
        self.cmd(0x11, &[]); // SLPOUT
        delay.delay_ms(120);

        self.cmd(0xCF, &[0x00, 0xC1, 0x30]);
        self.cmd(0xED, &[0x64, 0x03, 0x12, 0x81]);
        self.cmd(0xE8, &[0x85, 0x00, 0x78]);
        self.cmd(0xCB, &[0x39, 0x2C, 0x00, 0x34, 0x02]);
        self.cmd(0xF7, &[0x20]);
        self.cmd(0xEA, &[0x00, 0x00]);
        self.cmd(0xC0, &[0x23]); // PWCTR1
        self.cmd(0xC1, &[0x10]); // PWCTR2
        self.cmd(0xC5, &[0x3E, 0x28]); // VMCTR1
        self.cmd(0xC7, &[0x86]); // VMCTR2
        self.cmd(0x36, &[0x28]); // MADCTL: landscape (MV set), BGR
        self.cmd(0x3A, &[0x55]); // PIXFMT: 16 bit/pixel
        self.cmd(0xB1, &[0x00, 0x18]); // FRMCTR1
        self.cmd(0xB6, &[0x08, 0x82, 0x27]); // DFUNCTR
        self.cmd(0xF2, &[0x00]); // 3Gamma off
        self.cmd(0x26, &[0x01]); // Gamma curve 1
        self.cmd(
            0xE0,
            &[
                0x0F, 0x31, 0x2B, 0x0C, 0x0E, 0x08, 0x4E, 0xF1, 0x37, 0x07, 0x10, 0x03, 0x0E, 0x09,
                0x00,
            ],
        );
        self.cmd(
            0xE1,
            &[
                0x00, 0x0E, 0x14, 0x03, 0x11, 0x07, 0x31, 0xC1, 0x48, 0x08, 0x0F, 0x0C, 0x31, 0x36,
                0x0F,
            ],
        );

        self.cmd(0x11, &[]); // SLPOUT
        delay.delay_ms(120);
        self.cmd(0x29, &[]); // DISPON
        delay.delay_ms(20);
    }

    fn set_window(&mut self, x0: u16, y0: u16, x1: u16, y1: u16) {
        self.cmd(0x2A, &[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8]);
        self.cmd(0x2B, &[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8]);
    }

    /// Fill a rectangle with a solid color, streamed by no-increment DMA.
    fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        if w == 0 || h == 0 || x >= WIDTH || y >= HEIGHT {
            return;
        }
        let x1 = (x + w - 1).min(WIDTH - 1);
        let y1 = (y + h - 1).min(HEIGHT - 1);
        self.set_window(x, y, x1, y1);

        self.dc.set_low();
        self.spi.blocking_write(&[0x2Cu8]).unwrap(); // RAMWR
        self.dc.set_high();

        let count = (x1 - x + 1) as u32 * (y1 - y + 1) as u32;
        self.dma_fill(color, count);
    }

    /// Stream `count` copies of a single 16-bit `color` to the panel using
    /// memory-to-peripheral DMA with no memory increment (no frame buffer).
    fn dma_fill(&mut self, color: u16, count: u32) {
        use hal::pac::SPI1;

        // Switch the SPI to 16-bit frames (one pixel per word) and enable TX DMA.
        SPI1.ctlr1().modify(|w| w.set_spe(false));
        SPI1.ctlr1().modify(|w| w.set_dff(true));
        SPI1.ctlr2().modify(|w| w.set_txdmaen(true));
        SPI1.ctlr1().modify(|w| w.set_spe(true));

        let datar = SPI1.datar().as_ptr() as *mut u16;
        let mut remaining = count;
        while remaining > 0 {
            // A single DMA transfer is limited to 65535 items.
            let n = remaining.min(0xFFFF) as usize;
            unsafe {
                Transfer::new_write_repeated::<u16>(
                    self.dma.reborrow(),
                    Default::default(),
                    &color,
                    n,
                    datar,
                    TransferOptions::default(),
                )
                .blocking_wait();
            }
            remaining -= n as u32;
        }

        // Wait for the shift register to drain, then restore 8-bit framing so
        // the next command/pixel write (which is 8-bit) behaves correctly.
        while SPI1.statr().read().bsy() {}
        SPI1.ctlr1().modify(|w| w.set_spe(false));
        SPI1.ctlr2().modify(|w| w.set_txdmaen(false));
        SPI1.ctlr1().modify(|w| w.set_dff(false));
    }

    fn fill_screen(&mut self, color: u16) {
        self.fill_rect(0, 0, WIDTH, HEIGHT, color);
    }

}

impl OriginDimensions for Ili9341 {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for Ili9341 {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels {
            let (x, y) = (coord.x, coord.y);
            if x < 0 || y < 0 || x >= WIDTH as i32 || y >= HEIGHT as i32 {
                continue;
            }
            let x = x as u16;
            let y = y as u16;
            self.set_window(x, y, x, y);
            self.dc.set_low();
            self.spi.blocking_write(&[0x2Cu8]).unwrap(); // RAMWR
            self.dc.set_high();
            self.spi
                .blocking_write(&color.into_storage().to_be_bytes())
                .unwrap();
        }
        Ok(())
    }
}

// Screen is split into three horizontal zones along the 240 px (vertical) axis:
//   [0 .. TOP_H)                 top fixed text area
//   [TOP_H .. TOP_H + MID_H)     scrolling color-bar area
//   [HEIGHT - BOT_H .. HEIGHT)   bottom fixed text area
const TOP_H: u16 = 50;
const BOT_H: u16 = 50;
const MID_Y: u16 = TOP_H;
const MID_H: u16 = HEIGHT - TOP_H - BOT_H;

// Rainbow palette in RGB565. 8 bands tile the 320 px width exactly (40 px each),
// so the scrolling pattern wraps seamlessly.
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
const BAR_W: u16 = WIDTH / BARS.len() as u16;

/// Repaint the middle (scrolling) zone with the bar pattern shifted right by
/// `offset` px, wrapping around. Only the middle band is touched, so the fixed
/// top/bottom text is never overwritten. Each run is one no-increment DMA fill.
fn draw_bars(display: &mut Ili9341, offset: u16) {
    let mut x = 0u16;
    while x < WIDTH {
        let pos = (x + offset) % WIDTH;
        let band = (pos / BAR_W) as usize % BARS.len();
        // Length until the next bar boundary, clipped to the screen edge.
        let seg = (BAR_W - pos % BAR_W).min(WIDTH - x);
        display.fill_rect(x, MID_Y, seg, MID_H, BARS[band]);
        x += seg;
    }
}

/// Draw the two fixed text zones once. The top and bottom bands are filled with
/// a solid background, then text is drawn on top. The scrolling loop never
/// touches these bands, so the text stays put and does not flicker.
fn draw_fixed(display: &mut Ili9341) {
    display.fill_rect(0, 0, WIDTH, TOP_H, BLACK);
    display.fill_rect(0, HEIGHT - BOT_H, WIDTH, BOT_H, BLACK);

    let title = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    Text::new("Hello CH32V003!", Point::new(8, 32), title)
        .draw(display)
        .unwrap();

    let subtitle = MonoTextStyle::new(&FONT_6X10, Rgb565::YELLOW);
    Text::new("ILI9341 + SPI DMA", Point::new(8, HEIGHT as i32 - 30), subtitle)
        .draw(display)
        .unwrap();
    Text::new(
        "PWM backlight sweep",
        Point::new(8, HEIGHT as i32 - 14),
        subtitle,
    )
    .draw(display)
    .unwrap();
}

#[qingke_rt::entry]
fn main() -> ! {
    hal::debug::SDIPrint::enable();

    // Run the core at 48 MHz (HSI 24 MHz -> PLL x2) so HCLK = 48 MHz. SPI1's
    // baud clock is HCLK with a minimum /2 divider, so this is what allows a
    // 24 MHz SPI clock (HCLK/2). The default config runs HCLK at only 8 MHz.
    let mut config = hal::Config::default();
    config.rcc = hal::rcc::Config::SYSCLK_FREQ_48MHZ_HSI;
    let p = hal::init(config);
    let mut delay = Delay;

    // Control lines.
    let mut cs = Output::new(p.PC0, Level::High, Default::default());
    let dc = Output::new(p.PC1, Level::High, Default::default());
    let mut rst = Output::new(p.PC2, Level::High, Default::default());

    // Backlight on PA1 = TIM1_CH2, PWM at 1 kHz for brightness control.
    let bl_pin = PwmPin::new_ch2::<0>(p.PA1);
    let mut backlight = SimplePwm::new(
        p.TIM1,
        None,
        Some(bl_pin),
        None,
        None,
        Hertz::khz(1),
        CountingMode::default(),
    );
    let bl_max = backlight.get_max_duty();
    backlight.enable(Channel::Ch2);
    backlight.set_duty(Channel::Ch2, bl_max); // start at 100%

    // Single device on the bus: hold CS asserted for the whole session.
    cs.set_low();

    // Hardware reset pulse.
    rst.set_low();
    delay.delay_ms(20);
    rst.set_high();
    delay.delay_ms(120);

    // SPI1 TX-only, blocking (commands/pixels). Bulk fills use DMA1_CH3
    // directly via no-increment transfers. SCK = PC5, MOSI = PC6.
    let mut spi_config = hal::spi::Config::default();
    spi_config.frequency = Hertz::mhz(24); // HCLK/2 = 48/2 = 24 MHz (max for this bus)
    let spi = Spi::new_blocking_txonly::<0>(p.SPI1, p.PC5, p.PC6, spi_config);

    let mut display = Ili9341::new(spi, dc, p.DMA1_CH3);
    hal::println!("display init ...");
    display.init(&mut delay);
    hal::println!("display init ok");

    display.fill_screen(BLACK);

    // Draw the fixed top/bottom text zones once; the loop never repaints them.
    draw_fixed(&mut display);

    // Each frame: repaint only the middle band with the bars shifted by `offset`
    // (software scroll). The fixed text stays put and does not flicker. `level`
    // sweeps the backlight brightness.
    let mut offset: u16 = 0;
    let mut level: i32 = 0;
    let mut step: i32 = 2;
    loop {
        offset = (offset + 4) % WIDTH;
        draw_bars(&mut display, offset);

        level += step;
        if level >= 100 {
            level = 100;
            step = -2;
        } else if level <= 0 {
            level = 0;
            step = 2;
        }
        backlight.set_duty(Channel::Ch2, bl_max * level as u32 / 100);
    }
}
