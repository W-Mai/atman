use base64::Engine;
use std::io::Write;

pub(crate) mod images;
pub(crate) mod writes;

static OUTPUT: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn try_output() -> Option<std::sync::MutexGuard<'static, ()>> {
    match OUTPUT.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

pub fn write_text(payload: &str) -> std::io::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let _output = OUTPUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(seq.as_bytes())?;
    stderr.flush()
}

pub fn write_html(html: &str, plain_text: &str) -> anyhow::Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_html(html, Some(plain_text))?;
    Ok(())
}

pub fn read_image_png() -> anyhow::Result<Vec<u8>> {
    let mut clipboard = arboard::Clipboard::new()?;
    let image = clipboard.get_image()?;
    encode_image(image)
}

fn encode_image(image: arboard::ImageData<'_>) -> anyhow::Result<Vec<u8>> {
    use image::ImageEncoder;

    let expected = image
        .width
        .checked_mul(image.height)
        .and_then(|size| size.checked_mul(4));
    anyhow::ensure!(
        image.width > 0 && image.height > 0 && expected == Some(image.bytes.len()),
        "Clipboard image dimensions do not match RGBA data"
    );
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
