//! Font discovery and loading.
//!
//! The renderer needs a regular and a bold face. A comic face is optional and
//! only used for the header. When no explicit path is configured the loader
//! searches a list of well known system locations.

use crate::config::FontConfig;
use crate::render::Font;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const REGULAR_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/comic-neue/ComicNeue-Regular.otf",
    "/Library/Fonts/Arial.ttf",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "C:\\Windows\\Fonts\\arial.ttf",
];

const BOLD_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/usr/share/fonts/truetype/freefont/FreeSansBold.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/comic-neue/ComicNeue-Bold.otf",
    "/Library/Fonts/Arial Bold.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    "C:\\Windows\\Fonts\\arialbd.ttf",
];

const COMIC_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/msttcorefonts/comicbd.ttf",
    "/usr/share/fonts/truetype/msttcorefonts/comic.ttf",
    "/usr/share/fonts/comic-neue/ComicNeue-Bold.otf",
    "/System/Library/Fonts/Supplemental/Comic Sans Bold.ttf",
    "C:\\Windows\\Fonts\\comicbd.ttf",
];

/// The set of fonts used for rendering.
pub struct Fonts {
    pub regular: Font,
    pub bold: Font,
    pub comic: Option<Font>,
}

impl Fonts {
    /// Load the configured fonts, falling back to auto-discovery.
    pub fn load(config: &FontConfig) -> Result<Self> {
        let regular = load_one(config.regular.as_deref(), REGULAR_CANDIDATES, "regular")?;
        let bold = load_one(config.bold.as_deref(), BOLD_CANDIDATES, "bold")?;
        let comic = match config.comic.as_deref() {
            Some(path) => match Font::from_bytes(std::fs::read(path).ok().unwrap_or_default()) {
                Ok(font) => Some(font),
                Err(_) => None,
            },
            None => find_first(COMIC_CANDIDATES)
                .and_then(|path| Font::from_bytes(std::fs::read(path).ok()?).ok()),
        };
        Ok(Self {
            regular,
            bold,
            comic,
        })
    }
}

fn load_one(explicit: Option<&Path>, candidates: &[&str], label: &str) -> Result<Font> {
    if let Some(path) = explicit {
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read {label} font {}", path.display()))?;
        return Font::from_bytes(data)
            .with_context(|| format!("failed to parse {label} font {}", path.display()));
    }
    let path = find_first(candidates).ok_or_else(|| {
        anyhow::anyhow!(
            "could not find a {label} font; install DejaVu fonts or set fonts.{label} in the config"
        )
    })?;
    let data = std::fs::read(&path)?;
    Font::from_bytes(data).with_context(|| format!("failed to parse font {}", path.display()))
}

fn find_first(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}
