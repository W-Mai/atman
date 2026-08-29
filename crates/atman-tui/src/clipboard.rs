use base64::Engine;
use std::io::Write;

pub fn write_osc52(payload: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let _ = std::io::stderr().write_all(seq.as_bytes());
    let _ = std::io::stderr().flush();
}

pub fn read_image_png() -> anyhow::Result<Vec<u8>> {
    use image::ImageEncoder;

    let mut clipboard = arboard::Clipboard::new()?;
    let image = clipboard.get_image()?;
    let width = u32::try_from(image.width)?;
    let height = u32::try_from(image.height)?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        image.bytes.as_ref(),
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(png)
}
