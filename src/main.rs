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
//! Pixel data is streamed with DMA (SPI1_TX = DMA1_CH3). The HAL's DMA write is
//! async, so we drive it with `embassy_futures::block_on` from a blocking main.

use embassy_futures::block_on;
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::text::Text;
use hal::delay::Delay;
use hal::gpio::{Level, Output};
use hal::mode::Async;
use hal::peripherals;
use hal::spi::Spi;
use hal::time::Hertz;
use hal::timer::low_level::CountingMode;
use hal::timer::simple_pwm::{PwmPin, SimplePwm};
use hal::timer::Channel;
use {ch32_hal as hal, panic_halt as _};

// Landscape orientation: 320 wide x 240 tall.
const WIDTH: u16 = 320;
const HEIGHT: u16 = 240;

// RGB565 colors.
const BLACK: u16 = 0x0000;
const RED: u16 = 0xF800;
const GREEN: u16 = 0x07E0;
const BLUE: u16 = 0x001F;
const WHITE: u16 = 0xFFFF;

struct Ili9341 {
    spi: Spi<'static, peripherals::SPI1, Async>,
    dc: Output<'static>,
}

impl Ili9341 {
    fn new(spi: Spi<'static, peripherals::SPI1, Async>, dc: Output<'static>) -> Self {
        Self { spi, dc }
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

    /// Fill a rectangle with a solid color. Pixel data goes out over DMA.
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

        // Small RAM-resident buffer pre-filled with the color, streamed by DMA.
        const N: usize = 128;
        let [hi, lo] = color.to_be_bytes();
        let mut buf = [0u8; N * 2];
        let mut i = 0;
        while i < N {
            buf[2 * i] = hi;
            buf[2 * i + 1] = lo;
            i += 1;
        }

        let mut count = (x1 - x + 1) as u32 * (y1 - y + 1) as u32;
        while count > 0 {
            let n = core::cmp::min(count, N as u32) as usize;
            block_on(self.spi.write(&buf[..n * 2])).unwrap();
            count -= n as u32;
        }
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

// Color bar palette, drawn left-to-right and rotated on each cycle.
const BARS: [u16; 4] = [RED, GREEN, BLUE, WHITE];
const BAR_W: u16 = WIDTH / BARS.len() as u16;

/// Draw the rotated color bars plus the transparent text overlay.
fn draw_scene(display: &mut Ili9341, rotation: usize) {
    for (i, _) in BARS.iter().enumerate() {
        let color = BARS[(i + rotation) % BARS.len()];
        display.fill_rect(i as u16 * BAR_W, 0, BAR_W, HEIGHT, color);
    }

    let title = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    Text::new("Hello CH32V003!", Point::new(8, 150), title)
        .draw(display)
        .unwrap();

    let subtitle = MonoTextStyle::new(&FONT_6X10, Rgb565::YELLOW);
    Text::new("ILI9341 + SPI DMA", Point::new(8, 175), subtitle)
        .draw(display)
        .unwrap();
    Text::new("PWM backlight sweep", Point::new(8, 195), subtitle)
        .draw(display)
        .unwrap();
}

#[qingke_rt::entry]
fn main() -> ! {
    hal::debug::SDIPrint::enable();
    let p = hal::init(hal::Config::default());
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

    // SPI1 in TX-only mode with DMA (SPI1_TX = DMA1_CH3). SCK = PC5, MOSI = PC6.
    let mut spi_config = hal::spi::Config::default();
    spi_config.frequency = Hertz::mhz(12);
    let spi = Spi::new_txonly::<0>(p.SPI1, p.PC5, p.PC6, p.DMA1_CH3, spi_config);

    let mut display = Ili9341::new(spi, dc);
    hal::println!("display init ...");
    display.init(&mut delay);
    hal::println!("display init ok");

    // Initial scene: color bars (80 px each across 320 px) + transparent text.
    let mut rotation = 0usize;
    display.fill_screen(BLACK);
    draw_scene(&mut display, rotation);

    // Sweep the backlight brightness up and down so dimming is visible.
    // Each time it bottoms out at 0 (screen dark), shift the bars right by one.
    let mut level: i32 = 0;
    let mut step: i32 = 2;
    loop {
        level += step;
        if level >= 100 {
            level = 100;
            step = -2;
        } else if level <= 0 {
            level = 0;
            step = 2;
            // Backlight is off here, so redraw the shifted bars unseen.
            rotation = (rotation + 1) % BARS.len();
            draw_scene(&mut display, rotation);
        }
        backlight.set_duty(Channel::Ch2, bl_max * level as u32 / 100);
        delay.delay_ms(15);
    }
}
