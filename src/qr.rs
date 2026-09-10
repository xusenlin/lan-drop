use anyhow::{Context, Result, ensure};
use qrcode::{Color, QrCode};
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// 浅绿码点直接融入深绿地址卡，保留至少四个模块的静区。
/// 仅在启动时生成，不需要编码 PNG；透明静区可延伸到外框之外。
pub fn encode(url: &str) -> Result<SharedPixelBuffer<Rgba8Pixel>> {
    const SIZE: usize = 108;
    const QUIET_ZONE: usize = 4;
    let code = QrCode::new(url.as_bytes()).context("Could not encode the sharing address")?;
    let scale = SIZE / (code.width() + QUIET_ZONE * 2);
    ensure!(scale > 0, "Sharing address is too long for the QR code");
    let margin = (SIZE - code.width() * scale) / 2;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(SIZE as u32, SIZE as u32);
    let pixels = buffer.make_mut_slice();
    // 透明背景透出地址卡的圆角底色，码点沿用地址卡的浅绿文字色。
    pixels.fill(Rgba8Pixel::new(0, 0, 0, 0));
    for y in 0..code.width() {
        for x in 0..code.width() {
            if code[(x, y)] == Color::Dark {
                for dy in 0..scale {
                    let start = (margin + y * scale + dy) * SIZE + margin + x * scale;
                    pixels[start..start + scale].fill(Rgba8Pixel::new(185, 213, 198, 255));
                }
            }
        }
    }
    Ok(buffer)
}
