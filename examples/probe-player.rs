//! Proves AVFoundation decodes into the exact buffer gpui's renderer asserts on:
//!     cargo run --example probe-player -- <video>
use reviewpad::player::{Player, pump};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

const REQUIRED: u32 = 0x34323066; // '420f', the format gpui asserts

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).expect("pass a video"));
    let started = Instant::now();
    let mut player = Player::open(&path).expect("could not open");
    println!("open          {:>7.0?}", started.elapsed());

    let ready = Instant::now();
    while !player.is_ready() && ready.elapsed() < Duration::from_secs(10) {
        if player.failed() {
            panic!("AVFoundation failed to load the item");
        }
        // The app has AppKit's run loop; a harness has to lend one.
        pump(0.02);
    }
    println!(
        "ready         {:>7.0?}  {:.1}s at {:.2}fps",
        ready.elapsed(),
        player.duration(),
        player.fps()
    );

    // Seek somewhere far in — the case ffmpeg made expensive.
    let seek = Instant::now();
    player.seek(player.duration() / 2.);
    player.play();

    let mut frames = 0;
    let mut format_seen = None;
    let mut first = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(buffer) = player.frame() {
            if first.is_none() {
                first = Some(seek.elapsed());
            }
            format_seen = Some((
                buffer.get_pixel_format(),
                buffer.get_width(),
                buffer.get_height(),
            ));
            frames += 1;
        }
        pump(0.004);
    }
    player.pause();

    println!("first frame   {:>7.0?}", first.expect("no frame arrived"));
    let (format, width, height) = format_seen.expect("no buffer");
    println!("frames in 3s  {frames:>7}  ({:.0}/s)", frames as f64 / 3.);
    println!("buffer        {width}x{height}  format {format:#x}");
    assert_eq!(
        format, REQUIRED,
        "gpui's renderer would assert on this format"
    );
    println!("format matches what gpui binds — zero conversion");
}
