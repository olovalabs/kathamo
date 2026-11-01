//! Image attachments for the Kathamo bottom input bar.
//!
//! Attachments are kept as files on disk (dropped files keep their original
//! path, clipboard images are written to a session temp directory) so the
//! GPU image element can load them lazily and the agent CLI can receive a
//! plain path in the prompt.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{Image, ImageFormat};

/// Never keep more than this many attachments on one input bar.
pub const MAX_ATTACHMENTS: usize = 8;

/// Reject files larger than this; agents choke on giant prompts.
const MAX_IMAGE_BYTES: u64 = 40 * 1024 * 1024;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One image attached to a pane's input bar.
#[derive(Clone, Debug)]
pub struct AttachedImage {
    pub path: PathBuf,
    pub name: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl AttachedImage {
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        Some((self.width?, self.height?))
    }
}

/// Build an attachment from a path on disk, validating that it really is an
/// image (magic bytes win over the extension, so screenshots saved without an
/// extension still work).
pub fn from_path(path: &Path) -> Option<AttachedImage> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_IMAGE_BYTES {
        return None;
    }

    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let dimensions = probe_dimensions(&path)?;

    Some(AttachedImage {
        name: file_name(&path),
        path,
        width: dimensions.map(|(width, _)| width),
        height: dimensions.map(|(_, height)| height),
    })
}

/// Persist a clipboard image into the attachment directory and return it as
/// an attachment.
pub fn from_clipboard_image(image: &Image) -> Option<AttachedImage> {
    let extension = extension_for(image.format);
    let directory = attachment_directory();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!("pasted-{stamp}-{sequence}.{extension}"));
    std::fs::write(&path, &image.bytes).ok()?;

    let mut attachment = from_path(&path)?;
    attachment.name = "Pasted image".to_string();
    Some(attachment)
}

/// Add an attachment to the list unless it is already there or the list is
/// full. Returns whether it was added.
pub fn push_unique(attachments: &mut Vec<AttachedImage>, attachment: AttachedImage) -> bool {
    if attachments.len() >= MAX_ATTACHMENTS {
        return false;
    }
    if attachments
        .iter()
        .any(|existing| existing.path == attachment.path)
    {
        return false;
    }
    attachments.push(attachment);
    true
}

/// One image path, quoted when it contains spaces. Agent CLIs auto-attach an
/// image path when it arrives as a paste, so this is delivered with bracketed
/// paste rather than typed text.
pub fn prompt_path(path: &Path) -> String {
    quote_path(path)
}

/// Interpret pasted text as one or more local image paths. File managers and
/// screenshot tools often put a plain path (or a `file://` URI) on the
/// clipboard instead of pixel data; treating it as an attachment avoids
/// pasting a path where an image was meant.
pub fn from_clipboard_text(text: &str) -> Vec<AttachedImage> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if let Some(attachment) = attachment_from_token(trimmed) {
        return vec![attachment];
    }

    let mut attachments = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(attachment) = attachment_from_token(line) {
            attachments.push(attachment);
        }
    }

    let mut unique: Vec<AttachedImage> = Vec::new();
    for attachment in attachments {
        push_unique(&mut unique, attachment);
    }
    unique
}

fn attachment_from_token(token: &str) -> Option<AttachedImage> {
    let token = token.trim();
    let token = token
        .strip_prefix("file://")
        .or_else(|| token.strip_prefix("file:"))
        .unwrap_or(token);
    let token = token.trim_matches(|character| character == '\'' || character == '"');
    let token = token.replace("\\ ", " ");
    if token.is_empty() {
        return None;
    }

    from_path(Path::new(&percent_decode(&token)))
}

/// Cheap check used by drag-hover styling and the drop predicate: either a
/// known image extension or a recognizable image header.
pub fn looks_like_image(path: &Path) -> bool {
    has_image_extension(path) || has_image_magic(path)
}

fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "svg"
            )
        })
}

fn has_image_magic(path: &Path) -> bool {
    let Some(header) = read_header(path) else {
        return false;
    };

    header.starts_with(b"\x89PNG\r\n\x1a\n")
        || header.starts_with(b"\xff\xd8\xff")
        || header.starts_with(b"GIF8")
        || (header.starts_with(b"RIFF") && header.get(8..12) == Some(b"WEBP"))
        || header.starts_with(b"BM")
        || header.starts_with(b"II*\0")
        || header.starts_with(b"MM\0*")
        || has_svg_tag(&header)
}

fn read_header(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = vec![0u8; 1024];
    let read = file.read(&mut header).ok()?;
    header.truncate(read);
    Some(header)
}

fn has_svg_tag(header: &[u8]) -> bool {
    let text = String::from_utf8_lossy(header).to_ascii_lowercase();
    text.contains("<svg")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "image".to_string())
}

fn attachment_directory() -> PathBuf {
    let directory = std::env::temp_dir().join("kathamo-attachments");
    let _ = std::fs::create_dir_all(&directory);
    directory
}

fn extension_for(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
    }
}

fn probe_dimensions(path: &Path) -> Option<Option<(u32, u32)>> {
    // SVGs are rendered by GPUI's SVG path and are not decoded by `image`.
    if is_svg(path) {
        return Some(None);
    }

    let reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok().map(Some)
}

fn is_svg(path: &Path) -> bool {
    let has_svg_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"));
    has_svg_extension || read_header(path).is_some_and(|header| has_svg_tag(&header))
}

fn quote_path(path: &Path) -> String {
    let path = path.display().to_string();
    if path.chars().any(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path
    }
}

fn percent_decode(input: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex),
                bytes.get(index + 2).copied().and_then(hex),
            )
        {
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kathamo-attachment-test-{}-{name}",
            std::process::id()
        ))
    }

    fn write_png(path: &Path, width: u32, height: u32) {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        image
            .save_with_format(path, image::ImageFormat::Png)
            .expect("write test png");
    }

    #[test]
    fn reads_dimensions_from_a_png() {
        let path = temp_path("dimensions.png");
        write_png(&path, 3, 2);

        let attachment = from_path(&path).expect("png should be accepted");
        assert_eq!(attachment.dimensions(), Some((3, 2)));
        assert!(attachment.name.ends_with("dimensions.png"));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn accepts_extensionless_images_by_magic_bytes() {
        let path = temp_path("screenshot");
        write_png(&path, 1, 1);

        assert!(looks_like_image(&path));
        assert!(from_path(&path).is_some());

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_non_image_files() {
        let path = temp_path("notes.txt");
        std::fs::write(&path, b"not an image").unwrap();

        assert!(!looks_like_image(&path));
        assert!(from_path(&path).is_none());

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn prompt_path_quotes_paths_with_spaces() {
        assert_eq!(prompt_path(Path::new("/tmp/a.png")), "/tmp/a.png");
        assert_eq!(prompt_path(Path::new("/tmp/a b.png")), "\"/tmp/a b.png\"");
    }

    #[test]
    fn clipboard_path_text_becomes_an_attachment() {
        let path = temp_path("clipboard path.png");
        write_png(&path, 2, 2);
        let displayed = path.display().to_string();

        let plain = from_clipboard_text(&displayed);
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].path, std::fs::canonicalize(&path).unwrap());

        let uri = format!("file://{}", displayed.replace(' ', "%20"));
        assert_eq!(from_clipboard_text(&uri).len(), 1);
        assert_eq!(from_clipboard_text(&format!("\"{displayed}\"")).len(), 1);
        assert!(from_clipboard_text("just some text").is_empty());

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn push_unique_dedupes_and_caps_the_list() {
        let make = |index: usize| AttachedImage {
            path: PathBuf::from(format!("/tmp/{index}.png")),
            name: String::new(),
            width: None,
            height: None,
        };

        let mut attachments = Vec::new();
        assert!(push_unique(&mut attachments, make(0)));
        assert!(!push_unique(&mut attachments, make(0)));
        for index in 1..MAX_ATTACHMENTS {
            assert!(push_unique(&mut attachments, make(index)));
        }
        assert!(!push_unique(&mut attachments, make(MAX_ATTACHMENTS)));
        assert_eq!(attachments.len(), MAX_ATTACHMENTS);
    }
}
