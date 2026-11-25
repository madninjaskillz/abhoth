//! Port of tests/OpenOrca.Tools.Tests/PiWindowCaptureTests.cs, PiJobScreenshotSizesTests.cs and
//! PiJobScreenshotStepsTests.cs: key parsing, the PNG encoder, cropping/downscaling, blank
//! detection, the process-tree walk, screenshot sizes and shot steps, the size check and the
//! final-verification checklist.
//!
//! Not ported: the window capture itself needs Win32 FFI (`unsafe`, forbidden in this
//! workspace), so `job screenshot` reports "not available" after its argument checks. Hence:
//! - `Screenshot_RejectsUnknownKeys_AndReportsAJobWithoutAWindow`: only the unknown-key half is
//!   ported; the "before it opened a window" half needs `FindWindow` on the job's process tree.
//! - `Screenshot_CapturesARealWindow_AndKeysReachIt` and
//!   `Screenshot_Resize_FlagsAProgramThatDrawsAtAFixedSize`: real window capture, key input and
//!   resizing (and OPENORCA_GUI_TESTS=1 on an interactive desktop in C#).

use std::sync::Arc;
use std::time::Duration;

use orca_abstractions::{Tool, ToolContext, ToolResult};
use orca_tools_pi::job_tool::{self, parse_sizes, shot_segments, size_mismatch};
use orca_tools_pi::subagent;
use orca_tools_pi::window_capture::{self as wc, PiWindowShot};
use orca_tools_pi::{PiBashTool, PiJobManager, PiJobTool};
use regex::Regex;
use serde_json::{Value, json};

#[allow(clippy::too_many_arguments)]
fn shot(
    title: &str,
    width: i32,
    height: i32,
    dominant_share: f64,
    window_width: i32,
    window_height: i32,
    undrawn_right: i32,
    undrawn_bottom: i32,
) -> PiWindowShot {
    PiWindowShot {
        title: title.into(),
        width,
        height,
        png: Vec::new(),
        dominant_share,
        window_width,
        window_height,
        undrawn_right,
        undrawn_bottom,
    }
}

fn big_endian(b: &[u8], o: usize) -> usize {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize
}

/// Inflates a zlib stream of stored (uncompressed) deflate blocks, which is what the Rust
/// encoder writes (C# uses ZLibStream, which reads any deflate stream).
fn inflate_stored(z: &[u8]) -> Vec<u8> {
    assert_eq!(0x78, z[0]);
    let mut out = Vec::new();
    let mut p = 2;
    loop {
        let header = z[p];
        assert_eq!(0, (header >> 1) & 3, "only stored blocks are decoded here");
        let len = u16::from_le_bytes([z[p + 1], z[p + 2]]) as usize;
        let nlen = u16::from_le_bytes([z[p + 3], z[p + 4]]);
        assert_eq!(!(len as u16), nlen);
        out.extend_from_slice(&z[p + 5..p + 5 + len]);
        p += 5 + len;
        if header & 1 == 1 {
            break;
        }
    }
    // Adler-32 trailer.
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &out {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    assert_eq!((b << 16) | a, big_endian(z, p) as u32);
    out
}

// ---- PiWindowCaptureTests -------------------------------------------------------------------

#[test]
fn parse_keys_names_letters_and_combos() {
    let keys = wc::parse_keys(Some("Right right,Tab g 7 Esc Shift+Tab F5 PageDown")).unwrap();
    assert_eq!(
        vec![0x27, 0x27, 0x09, b'G', b'7', 0x1B, 0x09, 0x74, 0x22],
        keys.iter().map(|k| k.vk).collect::<Vec<_>>()
    );
    assert!(keys[0].extended);
    assert!(!keys[2].extended);
    assert_eq!(vec![0x10], keys[6].modifiers);
    assert_eq!("Shift+Tab", keys[6].name);
    assert!(wc::parse_keys(Some("  ")).unwrap().is_empty());
}

#[test]
fn parse_keys_rejects_unknown_names() {
    for (text, expected_bad) in [
        ("Right Banana", "Banana"),
        ("Hyper+Tab", "Hyper+Tab"),
        ("F13", "F13"),
    ] {
        assert_eq!(Err(expected_bad.to_string()), wc::parse_keys(Some(text)));
    }
}

#[test]
fn encode_png_is_a_valid_png_with_the_same_pixels() {
    // 3x2 BGRA: red, green, blue / white, black, grey.
    let bgra: [u8; 24] = [
        0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, //
        255, 255, 255, 255, 0, 0, 0, 255, 128, 128, 128, 255,
    ];
    let png = wc::encode_png(&bgra, 3, 2);

    assert_eq!([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A], png[..8]);
    assert_eq!(b"IHDR", &png[12..16]);
    assert_eq!(3, big_endian(&png, 16));
    assert_eq!(2, big_endian(&png, 20));
    // IHDR CRC covers type + data.
    assert_eq!(big_endian(&png, 29) as u32, wc::crc32(&png[12..29]));

    let idat_len = big_endian(&png, 33);
    assert_eq!(b"IDAT", &png[37..41]);
    let raw = inflate_stored(&png[41..41 + idat_len]);
    let expected: [u8; 20] = [
        0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 255, 255, 255, 0, 0, 0, 128, 128, 128,
    ];
    assert_eq!(expected.as_slice(), raw.as_slice());
    assert_eq!(b"IEND", &png[png.len() - 8..png.len() - 4]);
}

#[test]
fn crc32_matches_the_known_check_value() {
    assert_eq!(0xCBF4_3926u32, wc::crc32(b"123456789"));
}

#[test]
fn crop_takes_the_client_area_and_halve_averages_blocks() {
    // 4x2 image, each pixel's blue channel = its index.
    let mut px = vec![0u8; 4 * 2 * 4];
    for i in 0..8 {
        px[i * 4] = i as u8;
    }
    let crop = wc::crop(&px, 4, 1, 0, 2, 2);
    assert_eq!(
        vec![1, 2, 5, 6],
        (0..4).map(|i| crop[i * 4]).collect::<Vec<_>>()
    );

    let (w, h, half) = wc::halve(&px, 4, 2);
    assert_eq!((2, 1), (w, h));
    assert_eq!((1 + 4 + 5) / 4, half[0]);
    assert_eq!((2 + 3 + 6 + 7) / 4, half[4]);
}

#[test]
fn dominant_share_flags_a_blank_window() {
    let blank = vec![0u8; 100 * 4];
    assert!(wc::is_uniform(&blank));
    let mut busy = vec![0u8; 100 * 4];
    for i in 0..100 {
        busy[i * 4] = i as u8;
    }
    assert!(!wc::is_uniform(&busy));
    assert!((wc::dominant_share(&busy) - 0.01).abs() < 0.0005);
}

/// w x h BGRA filled with `bg`, with a noisy block in the top-left drawn_w x drawn_h.
fn frame(w: i32, h: i32, drawn_w: i32, drawn_h: i32) -> Vec<u8> {
    let bg: u8 = 0x10;
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let drawn = x < drawn_w && y < drawn_h;
            px[i] = if drawn { (x * 7 + y) as u8 } else { bg };
            px[i + 1] = if drawn { (x ^ y) as u8 } else { bg + 3 };
            px[i + 2] = if drawn { (y * 5) as u8 } else { bg + 6 };
        }
    }
    px
}

fn looks(px: &[u8]) -> bool {
    let (r, b) = wc::undrawn_edges(px, 480, 270);
    shot("x", 480, 270, 0.4, 480, 270, r, b).looks_undrawn()
}

#[test]
fn undrawn_edges_flag_an_app_that_draws_at_a_fixed_size() {
    // Launcher bench, local run 2: at 1920x1080 only the top-left 1280x720 was drawn.
    let (right, bottom) = wc::undrawn_edges(&frame(480, 270, 320, 180), 480, 270);
    assert_eq!((160, 90), (right, bottom));
    let s = shot("Launcher", 480, 270, 0.4, 480, 270, right, bottom);
    assert!(s.looks_undrawn());
    let notes = job_tool::shot_notes(&s, Some((1920, 1080)), Some((1920, 1080)));
    assert!(
        notes.contains("Resized the window to 1920x1080 first."),
        "{notes}"
    );
    assert!(notes.contains("the right 160 px and bottom 90 px of the 480x270 window are one flat colour while the top-left 320x180 has content"), "{notes}");

    // A full window, a strip on one side only, and a blank window are not that.
    assert!(!looks(&frame(480, 270, 480, 270)));
    assert!(!looks(&frame(480, 270, 300, 270)));
    assert!(!looks(&frame(480, 270, 0, 0)));
    assert!(
        job_tool::shot_notes(
            &shot("x", 1, 1, 0.4, 0, 0, 0, 0),
            Some((1920, 1080)),
            Some((1600, 900))
        )
        .contains("would only go to 1600x900")
    );
}

#[test]
fn parse_size_takes_width_by_height() {
    for (text, expected) in [
        ("1920x1080", Some((1920, 1080))),
        ("1280 \u{00D7} 720", Some((1280, 720))),
        ("800,600", Some((800, 600))),
        ("big", None),
        ("10x10", None),
        ("", None),
    ] {
        assert_eq!(expected, wc::parse_size(Some(text)), "{text}");
    }
}

#[test]
fn descendants_walks_the_whole_tree_and_ignores_others() {
    // bash(10) -> dotnet(11) -> app(12); unrelated 20 -> 21; a self-parented 0.
    let tree = wc::descendants(10, &[(11, 10), (12, 11), (21, 20), (0, 0), (13, 12)]);
    let mut ordered: Vec<u32> = tree.into_iter().collect();
    ordered.sort();
    assert_eq!(vec![10, 11, 12, 13], ordered);
}

async fn run(tool: &dyn Tool, args: Value, ctx: &ToolContext) -> ToolResult {
    let prepared = tool
        .prepare_arguments(&args.to_string())
        .expect("Pi tools validate their arguments")
        .unwrap_or_else(|e| panic!("{e}"));
    tool.execute(&serde_json::from_str(&prepared).unwrap(), ctx)
        .await
}

/// The unknown-key half of `Screenshot_RejectsUnknownKeys_AndReportsAJobWithoutAWindow` (see
/// the module docs for the no-window half).
#[tokio::test(flavor = "multi_thread")]
async fn screenshot_rejects_unknown_keys_and_reports_a_job_without_a_window() {
    if !cfg!(windows) {
        return; // screenshot is Windows-only
    }
    let Ok(shell) = orca_tools_pi::shell::resolve(None) else {
        return; // no bash available
    };
    let dir = tempfile::tempdir().unwrap();
    let jobs = Arc::new(PiJobManager::with_log_directory(dir.path().join("logs")));
    let ctx = ToolContext::new(dir.path());
    let bash = PiBashTool::new(Some(shell), None, 0, Vec::new(), jobs.clone());
    let started = run(
        &bash,
        // Long enough to outlive the 1 s start-up wait on a busy runner; killed at the end.
        json!({"command": "echo no window here; sleep 30", "background": true}),
        &ctx,
    )
    .await;
    let id = Regex::new(r"^Started job (\d+)")
        .unwrap()
        .captures(&started.content)
        .unwrap_or_else(|| panic!("{}", started.content))[1]
        .to_string();
    let tool = PiJobTool::new(jobs.clone());

    let bad_key = run(
        &tool,
        json!({"action": "screenshot", "id": id, "keys": "Right Warp"}),
        &ctx,
    )
    .await;
    assert!(bad_key.is_error);
    assert!(
        bad_key.content.contains("Unknown key \"Warp\""),
        "{}",
        bad_key.content
    );

    jobs.kill_all();
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[test]
fn schema_and_description_offer_screenshot() {
    let tool = PiJobTool::default();
    let schema = tool.parameter_schema();
    let props = schema["properties"].as_object().unwrap();
    assert!(props["action"].to_string().contains("screenshot"));
    assert!(props.contains_key("keys"));
    assert!(tool.description().contains("screenshot (Windows)"));
    assert!(props.contains_key("size"));
    // C# LookRule is VerifyChecklist.
    assert!(subagent::VERIFY_CHECKLIST.contains("bigger window"));
    assert!(subagent::VERIFY_CHECKLIST.contains("kill: true"));
    assert!(props.contains_key("kill"));
}

// ---- PiJobScreenshotSizesTests --------------------------------------------------------------

#[test]
fn sizes_parse() {
    for (text, count) in [
        (None, 0),
        (Some(""), 0),
        (Some("1920x1080"), 1),
        (Some("1920,1080"), 1),
        (Some("1920 1080"), 1),
        (Some("1280x720 1920x1080"), 2),
        (Some("1280x720, 1920x1080"), 2),
        (Some("800x600;1280x720;1920x1080"), 3),
    ] {
        let sizes = parse_sizes(text).unwrap_or_else(|bad| panic!("{text:?}: {bad}"));
        assert_eq!(count, sizes.len(), "{text:?}");
    }
}

#[test]
fn two_sizes_keep_their_order() {
    let sizes = parse_sizes(Some("1280x720 1920x1080")).unwrap();
    assert_eq!((1280, 720), sizes[0]);
    assert_eq!((1920, 1080), sizes[1]);
}

#[test]
fn bad_size_is_named() {
    for (text, bad) in [
        ("big", "big"),
        ("1280x720 huge", "huge"),
        ("10x10", "10x10"),
    ] {
        assert_eq!(Err(bad.to_string()), parse_sizes(Some(text)), "{text}");
    }
}

#[test]
fn too_many_sizes_are_refused() {
    assert!(parse_sizes(Some("800x600 1024x768 1280x720 1920x1080")).is_err());
}

// ---- PiJobScreenshotStepsTests --------------------------------------------------------------

fn names(keys: &[wc::PiKey]) -> Vec<&str> {
    keys.iter().map(|k| k.name.as_str()).collect()
}

#[test]
fn no_marker_is_one_step_with_all_the_keys() {
    let segments = shot_segments(Some("Right Right Tab")).unwrap();
    assert_eq!(1, segments.len());
    assert_eq!(vec!["Right", "Right", "Tab"], names(&segments[0]));
}

#[test]
fn no_keys_is_one_shot() {
    let segments = shot_segments(None).unwrap();
    assert_eq!(1, segments.len());
    assert!(segments[0].is_empty());
}

#[test]
fn markers_split_the_keys_leading_shot_is_the_start_screen_trailing_keys_get_a_shot() {
    let segments = shot_segments(Some("shot Right shot G Tab shot Esc")).unwrap();
    assert_eq!(4, segments.len());
    assert!(segments[0].is_empty());
    assert_eq!(vec!["Right"], names(&segments[1]));
    assert_eq!(2, segments[2].len());
    assert_eq!(vec!["Esc"], names(&segments[3]));
}

#[test]
fn trailing_shot_adds_no_empty_step() {
    let segments = shot_segments(Some("shot Right shot")).unwrap();
    assert_eq!(2, segments.len());
}

#[test]
fn unknown_key_is_reported() {
    assert_eq!(
        Err("NotAKey".to_string()),
        shot_segments(Some("shot Right shot NotAKey"))
    );
}

#[test]
fn size_mismatch_none_when_no_size_was_asked_or_it_matches() {
    let s = shot("x", 1920, 1080, 0.4, 1920, 1080, 0, 0);
    assert_eq!(None, size_mismatch(None, None, &s));
    assert_eq!(
        None,
        size_mismatch(Some((1920, 1080)), Some((1920, 1080)), &s)
    );
    // A DPI-scaled window can land a pixel off.
    assert_eq!(
        None,
        size_mismatch(Some((1921, 1079)), Some((1920, 1080)), &s)
    );
}

#[test]
fn size_mismatch_when_the_resize_did_not_take() {
    let s = shot("x", 1600, 900, 0.4, 1600, 900, 0, 0);
    let error = size_mismatch(Some((1920, 1080)), Some((1600, 900)), &s).unwrap();
    assert!(error.contains("asked for 1920x1080"));
    assert!(error.contains("1600x900"));
}

#[test]
fn size_mismatch_when_the_window_snapped_back_before_the_shot() {
    let s = shot("x", 1280, 720, 0.4, 1280, 720, 0, 0);
    let error = size_mismatch(Some((1920, 1080)), Some((1920, 1080)), &s).unwrap();
    assert!(error.contains("when the picture was taken"));
    assert!(error.contains("1280x720"));
}

#[test]
fn checklist_covers_each_view_and_kind_in_few_calls() {
    let text = subagent::VERIFY_CHECKLIST.as_str();
    for part in [
        "focus move",
        "secondary view",
        "filter or mode",
        "going back",
        "1280x720 1920x1080",
        "kill: true",
        "shot",
        "CLI tool",
        "error case",
        "Server",
        "endpoints",
        "what you checked",
        "never a key that launches",
    ] {
        assert!(text.contains(part), "{part}");
    }
    // `Assert.Equal(VerifyChecklist, LookRule)`: Rust has one constant for both (VERIFY_CHECKLIST).
}

#[test]
fn leads_and_the_delegated_check_use_the_checklist() {
    assert!(subagent::LEAD_INSTRUCTIONS.contains(subagent::VERIFY_CHECKLIST.as_str()));
    assert!(!subagent::VERIFY_CHECKLIST.contains("Do this once"));
}
