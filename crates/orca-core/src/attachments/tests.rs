//! Ported from tests/OpenOrca.Core.Tests/Attachments (AttachmentMessageTests, ClipboardTests,
//! WebPromptAttachmentsTests). The wire-format tests (`open_ai_compatible_*`) are in
//! tests/tool_images.rs, next to the client.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::clipboard::image_codec;
use super::clipboard::{
    ClipboardContent, ClipboardReader, ClipboardToolRunner, CommandClipboard, DirectoryClipboard,
    Platform, parse_apple_script_data,
};
use super::*;
use crate::client::tool_images;
use crate::client::{ContentPart, Role};

struct Dir(tempfile::TempDir);

impl Dir {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("temp dir"))
    }

    fn path(&self) -> String {
        self.0.path().to_string_lossy().into_owned()
    }

    fn write(&self, relative: &str, bytes: &[u8]) -> String {
        let path = self.0.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        // Path.GetFullPath: separators normalised.
        super::full_path("/", &path.to_string_lossy())
    }
}

fn png() -> Vec<u8> {
    image_codec::encode_png(1, 1, &[0, 0, 255, 255])
}

// ── AttachmentMessageTests: message building ───────────────────────────

#[test]
fn vision_image_goes_inline_and_the_chip_becomes_its_path() {
    let d = Dir::new();
    let png = png();
    let shot = PromptAttachment::for_path(1, d.write(".orca/attachments/paste-1.png", &png));

    let built = message_builder::build(
        &format!("what is wrong here {}", shot.chip()),
        std::slice::from_ref(&shot),
        true,
        &d.path(),
    );

    assert_eq!(
        "what is wrong here [image 1: .orca/attachments/paste-1.png]",
        built.text
    );
    assert_eq!(1, built.images_inline);
    assert_eq!(0, built.images_as_paths);
    assert_eq!(Role::User, built.message.role);
    assert_eq!(Some(built.text.clone()), built.message.content);
    let image = built.message.images().next().expect("an image part");
    assert_eq!("image/png", image.media_type);
    assert_eq!(png, image.data);
    assert_eq!(Some(shot.path.clone()), image.name);
}

#[test]
fn no_vision_image_goes_as_its_path_with_a_note() {
    let d = Dir::new();
    let shot = PromptAttachment::for_path(1, d.write("shot.png", &png()));

    let built = message_builder::build(
        &format!("{} fix this", shot.chip()),
        &[shot],
        false,
        &d.path(),
    );

    assert!(!built.message.has_images());
    assert!(built.text.starts_with("[image 1: shot.png] fix this"));
    assert!(built.text.contains(message_builder::NO_VISION_NOTE));
    assert_eq!(1, built.images_as_paths);
    assert_eq!(built.text, built.message.text());
}

#[test]
fn files_always_go_as_paths_and_a_file_outside_the_project_is_absolute() {
    let d = Dir::new();
    let outside_dir = Dir::new();
    let outside = outside_dir.write("orca-out.txt", b"x");
    let file = PromptAttachment::for_path(1, outside.clone());
    let built = message_builder::build(&format!("read {}", file.chip()), &[file], true, &d.path());
    assert_eq!(
        format!("read [file 1: {}]", outside.replace('\\', "/")),
        built.text
    );
    assert!(built.message.parts.is_empty());
}

#[test]
fn deleted_chip_drops_the_attachment() {
    let d = Dir::new();
    let shot = PromptAttachment::for_path(1, d.write("a.png", &png()));
    let built = message_builder::build("never mind", &[shot], true, &d.path());
    assert_eq!("never mind", built.message.text());
    assert!(built.message.parts.is_empty());
}

#[test]
fn vision_unsupported_or_too_large_image_falls_back_to_its_path() {
    let d = Dir::new();
    let bmp = PromptAttachment::for_path(1, d.write("old.bmp", &[1]));
    let built = message_builder::build(&bmp.chip(), std::slice::from_ref(&bmp), true, &d.path());
    assert_eq!(0, built.images_inline);
    assert_eq!(1, built.images_as_paths);
    assert_eq!("[image 1: old.bmp]", built.text); // the model can still use its tools on it
}

#[test]
fn chip_text_escapes_brackets_in_file_names() {
    let d = Dir::new();
    let a = PromptAttachment::for_path(3, Path::new(&d.path()).join("x[1].png").to_string_lossy());
    assert_eq!("[image 3: x(1).png]", a.chip());
    assert_eq!(
        1,
        chip_pattern()
            .find_iter(&format!("see {} ok", a.chip()))
            .count()
    );
}

// ── provider wire formats (the parts not needing a server) ─────────────

#[test]
fn open_ai_compatible_without_vision_image_part_becomes_a_path_note() {
    // The prompt was built for a vision model, then sent to one without (model switched).
    let d = Dir::new();
    let shot = PromptAttachment::for_path(1, d.write("s.png", &png()));
    let message =
        message_builder::build(&shot.chip(), std::slice::from_ref(&shot), true, &d.path()).message;

    let rewritten = tool_images::rewrite(std::slice::from_ref(&message), false);

    assert_eq!(1, rewritten.len());
    let m = &rewritten[0];
    assert!(!m.has_images());
    assert!(m.text().contains(&shot.path));
    assert!(message.has_images()); // the conversation's copy is unchanged
}

#[test]
fn open_ai_compatible_with_vision_user_images_pass_through() {
    let d = Dir::new();
    let shot = PromptAttachment::for_path(1, d.write("s.png", &png()));
    let message = message_builder::build(&shot.chip(), &[shot], true, &d.path()).message;
    assert_eq!(
        vec![message.clone()],
        tool_images::rewrite(std::slice::from_ref(&message), true)
    );
}

#[test]
fn open_ai_compatible_sends_the_image_as_an_image_url_data_uri_part() {
    // The body itself is checked against the fake server in tests/tool_images.rs.
    let d = Dir::new();
    let png = png();
    let shot = PromptAttachment::for_path(1, d.write("s.png", &png));
    let message =
        message_builder::build(&format!("look {}", shot.chip()), &[shot], true, &d.path()).message;
    use base64::Engine;
    assert_eq!(
        serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": "look [image 1: s.png]"},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&png))}}
        ]}),
        message.to_wire(false)
    );
}

// Anthropic_SendsTheImageAsAnImageBlock: not ported, the Rust port has no Anthropic client.

// ── dropped / pasted paths ─────────────────────────────────────────────

#[test]
fn pasted_path_quoted_windows_terminal_drop_is_recognised() {
    let d = Dir::new();
    let img = d.write("my shots/one.png", &[1]);
    assert_eq!(
        Some(vec![img.clone()]),
        pasted_path_detector::try_parse(&format!("\"{img}\""), &d.path(), true)
    );
    assert_eq!(
        Some(vec![img.clone()]),
        pasted_path_detector::try_parse(&format!("  \"{img}\" "), &d.path(), true)
    );
}

#[test]
fn pasted_path_several_files_unquoted_and_quoted() {
    let d = Dir::new();
    let a = d.write("a.png", &[1]);
    let b = d.write("b c.jpg", &[1]);
    assert_eq!(
        Some(vec![a.clone(), b.clone()]),
        pasted_path_detector::try_parse(&format!("{a} \"{b}\""), &d.path(), true)
    );
}

#[test]
fn pasted_path_relative_and_file_uri() {
    let d = Dir::new();
    let a = d.write("img/x.png", &[1]);
    assert_eq!(
        Some(vec![a.clone()]),
        pasted_path_detector::try_parse("img/x.png", &d.path(), false)
    );
    assert_eq!(
        Some(vec![a.clone()]),
        pasted_path_detector::try_parse(&file_uri_of(&a), &d.path(), false)
    );
}

/// `new Uri(path).AbsoluteUri` for the paths these tests use.
fn file_uri_of(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.replace(' ', "%20");
    if p.starts_with('/') {
        format!("file://{p}")
    } else {
        format!("file:///{p}")
    }
}

#[test]
fn pasted_path_ordinary_text_is_not_a_path() {
    let d = Dir::new();
    d.write("img/x.png", &[1]);
    d.write("x.png", &[1]);
    for text in [
        "hello world",
        "look at img/x.png please",
        "missing/file.png",
        "x.png", // a bare word is not a pasted path
        "",
    ] {
        assert_eq!(
            None,
            pasted_path_detector::try_parse(text, &d.path(), false),
            "{text}"
        );
    }
}

#[test]
fn pasted_path_images_only_rejects_other_files() {
    let d = Dir::new();
    let txt = d.write("notes.txt", &[1]);
    assert_eq!(
        None,
        pasted_path_detector::try_parse(&format!("\"{txt}\""), &d.path(), true)
    );
    assert_eq!(
        Some(vec![txt.clone()]),
        pasted_path_detector::try_parse(&format!("\"{txt}\""), &d.path(), false)
    );
}

// ── path -> link ───────────────────────────────────────────────────────

#[test]
fn file_uri_is_percent_encoded() {
    for (path, expected) in [
        (
            r"C:\Users\me\a b\x[1].cs",
            "file:///C:/Users/me/a%20b/x%5B1%5D.cs",
        ),
        ("/home/me/naïve.txt", "file:///home/me/na%C3%AFve.txt"),
        (r"C:\x#y%z.md", "file:///C:/x%23y%25z.md"),
    ] {
        assert_eq!(expected, file_link_resolver::to_file_uri(path));
    }
}

#[test]
fn resolve_relative_path_with_line_suffix_and_quotes() {
    let d = Dir::new();
    let file = d.write("src/app.cs", &[1]);
    let wd = d.path();
    let r = |t: &str| file_link_resolver::resolve(t, &wd);
    assert_eq!(Some(file.clone()), r("src/app.cs"));
    assert_eq!(Some(file.clone()), r("src/app.cs:12"));
    assert_eq!(Some(file.clone()), r("src/app.cs:10-40"));
    assert_eq!(Some(file.clone()), r("`src/app.cs`"));
    assert_eq!(None, r("src/missing.cs"));
    assert_eq!(None, r("https://example.com/src/app.cs"));
}

#[test]
fn find_paths_links_existing_files_in_prose_only() {
    let d = Dir::new();
    let app = d.write("src/app.cs", &[1]);
    d.write("README.md", &[1]);
    let text = "I changed src/app.cs:12 and README.md, not src/nope.cs or v1.2.3 or e.g. this.";

    let refs = file_link_resolver::find_paths(text, &d.path());

    assert_eq!(
        vec!["src/app.cs:12", "README.md"],
        refs.iter()
            .map(|r| &text[r.index..r.index + r.length])
            .collect::<Vec<_>>()
    );
    assert_eq!(app, refs[0].full_path);
    assert!(refs[0].uri().starts_with("file:///"));
}

#[test]
fn find_paths_absolute_windows_path() {
    let d = Dir::new();
    let file = d.write("deep/x.txt", &[1]);
    let text = format!("Saved to {file}.");
    let other = Dir::new();
    let refs = file_link_resolver::find_paths(&text, &other.path());
    assert_eq!(1, refs.len());
    assert_eq!(file, &text[refs[0].index..refs[0].index + refs[0].length]);
}

// ── ClipboardTests ─────────────────────────────────────────────────────

struct FakeClipboard {
    content: ClipboardContent,
    reads: AtomicUsize,
}

impl FakeClipboard {
    fn new(content: ClipboardContent) -> Self {
        Self {
            content,
            reads: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ClipboardReader for FakeClipboard {
    async fn read(&self) -> Result<ClipboardContent, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.content.clone())
    }
}

/// A packed DIB: BITMAPINFOHEADER (+ optional masks), then the rows.
fn dib(
    width: i32,
    height: i32,
    bits: u16,
    compression: u32,
    pixels: &[u8],
    masks: &[u32],
) -> Vec<u8> {
    let mut header = vec![0u8; 40];
    header[0..4].copy_from_slice(&40i32.to_le_bytes());
    header[4..8].copy_from_slice(&width.to_le_bytes());
    header[8..12].copy_from_slice(&height.to_le_bytes());
    header[12..14].copy_from_slice(&1u16.to_le_bytes());
    header[14..16].copy_from_slice(&bits.to_le_bytes());
    header[16..20].copy_from_slice(&compression.to_le_bytes());
    for m in masks {
        header.extend_from_slice(&m.to_le_bytes());
    }
    header.extend_from_slice(pixels);
    header
}

#[test]
fn dib24_bottom_up_decodes_to_rgba_top_row_first() {
    // 2x2, bottom row first, BGR, rows padded to 4 bytes: bottom = blue, white; top = red, green.
    let pixels = [
        255, 0, 0, 255, 255, 255, 0, 0, //
        0, 0, 255, 0, 255, 0, 0, 0,
    ];
    let image = image_codec::decode_dib(&dib(2, 2, 24, 0, &pixels, &[])).expect("decodes");
    assert_eq!((2, 2), (image.width, image.height));
    assert_eq!(
        vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255
        ],
        image.rgba
    );
}

#[test]
fn dib32_with_all_zero_alpha_is_opaque() {
    // Screenshots copied by most apps: 32-bit BI_RGB whose fourth byte is padding (0).
    let image = image_codec::decode_dib(&dib(1, 1, 32, 0, &[10, 20, 30, 0], &[])).unwrap();
    assert_eq!(vec![30, 20, 10, 255], image.rgba);
}

#[test]
fn dib32_bitfields_uses_the_masks() {
    // Masks put red in the low byte (RGBA order in memory).
    let image = image_codec::decode_dib(&dib(
        1,
        1,
        32,
        3,
        &[1, 2, 3, 0],
        &[0x0000_00FF, 0x0000_FF00, 0x00FF_0000],
    ))
    .unwrap();
    assert_eq!(vec![1, 2, 3, 255], image.rgba);
}

#[test]
fn dib8_uses_the_palette() {
    let mut header = dib(1, 1, 8, 0, &[], &[]);
    header[32..36].copy_from_slice(&2u32.to_le_bytes()); // two palette entries
    header.extend_from_slice(&[
        0, 0, 0, 0, /* entry 1: BGR0 */ 9, 8, 7, 0, /* pixel = 1, padded */ 1, 0, 0, 0,
    ]);
    assert_eq!(
        vec![7, 8, 9, 255],
        image_codec::decode_dib(&header).unwrap().rgba
    );
}

#[test]
fn dib_garbage_is_rejected() {
    assert_eq!(None, image_codec::dib_to_png(&[1, 2, 3])); // too short
}

#[test]
fn dib_compressed_is_rejected() {
    assert_eq!(
        None,
        image_codec::decode_dib(&dib(1, 1, 8, 1 /* RLE8 */, &[0, 0, 0, 0], &[]))
    );
}

#[test]
fn encode_png_writes_a_valid_png() {
    let rgba = [255, 0, 0, 255, 0, 255, 0, 128];
    let png = image_codec::encode_png(2, 1, &rgba);

    assert!(image_codec::is_png(&png));
    assert_eq!(b"IHDR", &png[12..16]);
    assert_eq!(2, i32::from_be_bytes(png[16..20].try_into().unwrap()));
    assert_eq!(1, i32::from_be_bytes(png[20..24].try_into().unwrap()));
    assert_eq!(6, png[25]); // RGBA
    // IEND with its well-known CRC closes the file.
    assert_eq!(
        &[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82],
        &png[png.len() - 12..]
    );

    // IDAT inflates to one filter byte + the row.
    let idat_length = i32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
    assert_eq!(b"IDAT", &png[37..41]);
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&png[41..41 + idat_length])
        .read_to_end(&mut raw)
        .unwrap();
    let mut expected = vec![0u8];
    expected.extend_from_slice(&rgba);
    assert_eq!(expected, raw);
}

#[tokio::test]
async fn paste_image_is_saved_under_orca_attachments_with_a_gitignore_and_becomes_a_chip() {
    let d = Dir::new();
    let png = image_codec::encode_png(1, 1, &[1, 2, 3, 255]);
    let tray = AttachmentTray::new(AttachmentStore::with_clock(d.path(), || {
        chrono::NaiveDate::from_ymd_opt(2026, 11, 17)
            .unwrap()
            .and_hms_opt(22, 10, 5)
            .unwrap()
    }));
    let clip = FakeClipboard::new(ClipboardContent {
        png: Some(png.clone()),
        ..Default::default()
    });

    let result = tray.paste_from(&clip, &d.path()).await;

    let folder = Path::new(&d.path()).join(".orca").join("attachments");
    let saved = folder.join("paste-20261117-221005.png");
    assert_eq!("[image 1: paste-20261117-221005.png] ", result.insert);
    assert_eq!(png, std::fs::read(&saved).unwrap());
    assert!(
        std::fs::read_to_string(folder.join(".gitignore"))
            .unwrap()
            .contains('*')
    );
    let pending = tray.pending();
    assert_eq!(1, pending.len());
    assert_eq!(saved.to_string_lossy(), pending[0].path);
    assert!(result.notice.unwrap().contains("Attached image 1"));

    // A second paste in the same second gets its own file and the next number.
    let second = tray.paste_from(&clip, &d.path()).await;
    assert_eq!("[image 2: paste-20261117-221005-2.png] ", second.insert);
    assert_eq!(2, clip.reads.load(Ordering::SeqCst));
}

#[tokio::test]
async fn paste_files_attach_those_files_by_path() {
    let d = Dir::new();
    let a = d.write("notes.txt", b"x");
    let b = d.write("shot.jpg", &[1]);
    let gone = Path::new(&d.path())
        .join("gone.txt")
        .to_string_lossy()
        .into_owned();
    let tray = AttachmentTray::new(AttachmentStore::new(d.path()));

    let result = tray
        .paste_from(
            &FakeClipboard::new(ClipboardContent {
                files: Some(vec![a, b, gone]),
                ..Default::default()
            }),
            &d.path(),
        )
        .await;

    assert_eq!("[file 1: notes.txt] [image 2: shot.jpg] ", result.insert);
    assert_eq!(
        vec![AttachmentKind::File, AttachmentKind::Image],
        tray.pending().iter().map(|p| p.kind).collect::<Vec<_>>()
    );
    assert!(!Path::new(&d.path()).join(".orca").exists());
}

#[tokio::test]
async fn paste_text_is_inserted_as_is_but_an_image_path_becomes_a_chip() {
    let d = Dir::new();
    let tray = AttachmentTray::new(AttachmentStore::new(d.path()));
    let text = tray
        .paste_from(
            &FakeClipboard::new(ClipboardContent {
                text: Some("hello world".into()),
                ..Default::default()
            }),
            &d.path(),
        )
        .await;
    assert_eq!("hello world", text.insert);
    assert_eq!(None, text.notice);
    assert!(tray.pending().is_empty());

    let img = d.write("my shot.png", &[1]);
    let path = tray
        .paste_from(
            &FakeClipboard::new(ClipboardContent {
                text: Some(format!("\"{img}\"")),
                ..Default::default()
            }),
            &d.path(),
        )
        .await;
    assert_eq!("[image 1: my shot.png] ", path.insert);
}

#[tokio::test]
async fn paste_empty_or_failed_inserts_nothing_and_says_why() {
    let d = Dir::new();
    let tray = AttachmentTray::new(AttachmentStore::new(d.path()));
    let empty = tray
        .paste_from(&FakeClipboard::new(ClipboardContent::empty()), &d.path())
        .await;
    assert_eq!("", empty.insert);
    assert!(empty.notice.unwrap().contains("empty"));

    let failed = tray
        .paste_from(
            &FakeClipboard::new(ClipboardContent::failed(
                "Install xclip to paste from the clipboard.",
            )),
            &d.path(),
        )
        .await;
    assert_eq!("", failed.insert);
    assert!(failed.notice.unwrap().contains("xclip"));
}

#[test]
fn take_returns_only_attachments_whose_chip_is_still_there_and_resets() {
    let d = Dir::new();
    let dir = Path::new(&d.path()).to_path_buf();
    let tray = AttachmentTray::new(AttachmentStore::new(d.path()));
    let a = tray.add(&dir.join("a.png").to_string_lossy());
    tray.add(&dir.join("b.png").to_string_lossy());

    let used = tray.take(&format!("look at {} please", a.chip())); // b's chip was deleted

    assert_eq!(vec![a], used);
    assert!(tray.pending().is_empty());
    assert_eq!(1, tray.add(&dir.join("c.png").to_string_lossy()).number);
}

#[tokio::test]
async fn directory_clipboard_reads_files_then_image_then_text() {
    let d = Dir::new();
    let clip = DirectoryClipboard::new(d.path());
    assert!(clip.read().await.unwrap().is_empty());
    d.write("text.txt", b"hi");
    assert_eq!(Some("hi".to_string()), clip.read().await.unwrap().text);
    d.write("image.png", &[1, 2]);
    assert_eq!(Some(vec![1, 2]), clip.read().await.unwrap().png);
    d.write("files.txt", b"C:\\a.txt\n\n/b.txt\n");
    assert_eq!(
        Some(vec!["C:\\a.txt".to_string(), "/b.txt".to_string()]),
        clip.read().await.unwrap().files
    );
}

// ── macOS / Linux tools (faked) ────────────────────────────────────────

type Outputs = Vec<(&'static str, Option<(i32, Vec<u8>)>)>;

fn runner(
    outputs: Outputs,
    calls: Option<Arc<std::sync::Mutex<Vec<String>>>>,
) -> ClipboardToolRunner {
    let outputs = Arc::new(outputs);
    Arc::new(move |file: String, args: Vec<String>| {
        let mut key = file.clone();
        key.push(' ');
        key.push_str(&args.join(" "));
        if let Some(calls) = &calls {
            calls.lock().unwrap().push(key.clone());
        }
        let found = outputs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, r)| r.clone());
        let tool_missing = outputs.iter().any(|(k, _)| *k == file);
        Box::pin(async move {
            match found {
                Some(r) => r,
                None if tool_missing => None,
                None => Some((1, Vec::new())),
            }
        })
    })
}

#[tokio::test]
async fn wayland_prefers_files_then_png() {
    let png = image_codec::encode_png(1, 1, &[0, 0, 0, 255]);
    let clip = CommandClipboard::with_runner(
        Platform::Wayland,
        runner(
            vec![
                (
                    "wl-paste --list-types",
                    Some((0, b"image/png\ntext/plain\n".to_vec())),
                ),
                (
                    "wl-paste --no-newline --type image/png",
                    Some((0, png.clone())),
                ),
            ],
            None,
        ),
    );
    assert_eq!(Some(png), clip.read().await.unwrap().png);

    let files = CommandClipboard::with_runner(
        Platform::Wayland,
        runner(
            vec![
                (
                    "wl-paste --list-types",
                    Some((0, b"text/uri-list\nimage/png\n".to_vec())),
                ),
                (
                    "wl-paste --no-newline --type text/uri-list",
                    Some((
                        0,
                        b"# comment\nfile:///home/me/a%20b.png\nhttps://x.test/\n".to_vec(),
                    )),
                ),
            ],
            None,
        ),
    );
    let paths = files.read().await.unwrap().files.unwrap();
    assert_eq!(1, paths.len());
    assert!(paths[0].ends_with("a b.png"));
}

#[tokio::test]
async fn x11_falls_back_to_text() {
    let clip = CommandClipboard::with_runner(
        Platform::X11,
        runner(
            vec![
                (
                    "xclip -selection clipboard -t TARGETS -o",
                    Some((0, b"UTF8_STRING\nTARGETS\n".to_vec())),
                ),
                (
                    "xclip -selection clipboard -o",
                    Some((0, b"some text".to_vec())),
                ),
            ],
            None,
        ),
    );
    assert_eq!(
        Some("some text".to_string()),
        clip.read().await.unwrap().text
    );
}

#[tokio::test]
async fn missing_tool_gives_a_clear_message() {
    let clip = CommandClipboard::with_runner(Platform::X11, runner(vec![("xclip", None)], None));
    let content = clip.read().await.unwrap();
    assert!(content.is_empty());
    assert!(content.error.unwrap().contains("Install xclip"));
}

#[tokio::test]
async fn mac_os_without_pngpaste_reads_apple_script_png_data() {
    let png = image_codec::encode_png(1, 1, &[0, 0, 0, 255]);
    let hex: String = png.iter().map(|b| format!("{b:02X}")).collect();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let script_output = format!("«data PNGf{hex}»\n").into_bytes();
    let clip = CommandClipboard::with_runner(
        Platform::MacOS,
        runner(
            vec![
                ("pngpaste", None),
                (
                    "osascript -e the clipboard as «class PNGf»",
                    Some((0, script_output)),
                ),
            ],
            Some(calls.clone()),
        ),
    );

    assert_eq!(Some(png), clip.read().await.unwrap().png);
    assert!(calls.lock().unwrap().iter().any(|c| c == "pngpaste -"));
}

#[test]
fn parse_apple_script_data_cases() {
    assert_eq!(
        Some(vec![0x89, 0x50, 0x4E, 0x47]),
        parse_apple_script_data("«data PNGf89504E47»")
    );
    assert_eq!(None, parse_apple_script_data("nothing"));
    assert_eq!(None, parse_apple_script_data("«data PNGf8950Z»"));
}

// Rust only: the Windows reader (PowerShell) output.
#[test]
fn windows_output_is_parsed_in_clipboard_order() {
    use super::clipboard::parse_windows_output as parse_windows;
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let files = parse_windows(&format!(
        "FILES\r\n{}\r\n{}\r\n",
        b64("C:\\a b.txt".as_bytes()),
        b64("C:\\ü.png".as_bytes())
    ));
    assert_eq!(
        Some(vec!["C:\\a b.txt".to_string(), "C:\\ü.png".to_string()]),
        files.files
    );
    let png = png();
    assert_eq!(
        Some(png.clone()),
        parse_windows(&format!("PNG\n{}\n", b64(&png))).png
    );
    let dib = dib(1, 1, 32, 0, &[10, 20, 30, 0], &[]);
    let from_dib = parse_windows(&format!("DIB\n{}\n", b64(&dib))).png.unwrap();
    assert!(image_codec::is_png(&from_dib));
    assert_eq!(
        Some("hé".to_string()),
        parse_windows(&format!("TEXT\n{}\n", b64("hé".as_bytes()))).text
    );
    assert!(parse_windows("EMPTY\n").is_empty());
}

// ── WebPromptAttachmentsTests ──────────────────────────────────────────

fn web_root() -> Dir {
    use base64::Engine;
    let d = Dir::new();
    // A 1x1 PNG.
    d.write(
        ".orca/attachments/a.png",
        &base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
            .unwrap(),
    );
    d.write("notes.txt", b"hi");
    d
}

#[test]
fn composed_list_comes_back_as_attachments() {
    let root = web_root();
    let prompt = web_prompt_attachments::compose(
        "look at this",
        &[".orca/attachments/a.png".into(), "notes.txt".into()],
    );
    let (text, attachments) = web_prompt_attachments::extract(&prompt, &root.path());

    assert_eq!(2, attachments.len());
    assert_eq!(AttachmentKind::Image, attachments[0].kind);
    assert_eq!(AttachmentKind::File, attachments[1].kind);
    assert!(text.contains("[image 1: a.png]"));
    assert!(text.starts_with("look at this\n\nAttached files"));

    // With vision the image goes as an image part; the text names its path.
    let built = message_builder::build(&text, &attachments, true, &root.path());
    assert_eq!(1, built.images_inline);
    assert_eq!(1, built.message.images().count());
    assert!(built.text.contains("a.png"));

    // Without vision it stays a path.
    let plain = message_builder::build(&text, &attachments, false, &root.path());
    assert!(
        !plain
            .message
            .parts
            .iter()
            .any(|p| matches!(p, ContentPart::Image(_)))
    );
    assert_eq!(1, plain.images_as_paths);
}

#[test]
fn leaves_other_text_alone() {
    let root = web_root();
    for prompt in [
        "just text",
        "Attached files:\n- missing.png",
        "Attached files:\n- notes.txt\nand then more text",
        "I typed Attached files:\n- notes.txt",
    ] {
        let (text, attachments) = web_prompt_attachments::extract(prompt, &root.path());
        assert_eq!(prompt, text);
        assert!(attachments.is_empty());
    }
}
