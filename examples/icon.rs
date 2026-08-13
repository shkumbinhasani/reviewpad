//! Rasterize `assets/icon.svg` into the PNG set macOS wants, then leave
//! `iconutil` to pack it:
//!
//! ```sh
//! cargo run --example icon
//! iconutil -c icns assets/ReviewPad.iconset -o assets/ReviewPad.icns
//! ```
//!
//! `scripts/bundle-macos.sh` does both. This uses resvg — the same renderer
//! gpui rasterizes the app's SVGs with — so the icon cannot drift from what the
//! UI would draw.

use std::{fs, path::PathBuf};

/// Every size an `.icns` carries, as (pixels, file stem).
const SIZES: &[(u32, &str)] = &[
    (16, "icon_16x16"),
    (32, "icon_16x16@2x"),
    (32, "icon_32x32"),
    (64, "icon_32x32@2x"),
    (128, "icon_128x128"),
    (256, "icon_128x128@2x"),
    (256, "icon_256x256"),
    (512, "icon_256x256@2x"),
    (512, "icon_512x512"),
    (1024, "icon_512x512@2x"),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read(root.join("assets/icon.svg"))?;
    let tree = usvg::Tree::from_data(&source, &usvg::Options::default())?;

    let iconset = root.join("assets/ReviewPad.iconset");
    fs::create_dir_all(&iconset)?;

    let natural = tree.size();
    for (pixels, name) in SIZES {
        let mut pixmap = tiny_skia::Pixmap::new(*pixels, *pixels)
            .ok_or_else(|| format!("could not allocate a {pixels}px canvas"))?;
        let scale = *pixels as f32 / natural.width();
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let path = iconset.join(format!("{name}.png"));
        pixmap.save_png(&path)?;
        println!("{pixels:>4}px  {}", path.display());
    }

    Ok(())
}
