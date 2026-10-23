//! Comic page renderer.
//!
//! The layout follows the pages of Microsoft Comic Chat: the conversation is
//! drawn as a grid of panels. One panel shows one cell, a group of messages
//! that arrived within the same short window. Every panel has a caption box in
//! the top left corner naming the speakers, their characters stand in the
//! panel and every message is spoken in a bubble that overlaps the characters.
//! A sidebar on the right lists the characters of the conversation.

use crate::config::ComicConfig;
use crate::fonts::Fonts;
use crate::render::{fit_rgba, Canvas, Color, Font};
use image::RgbaImage;
use std::path::PathBuf;

/// Padding between the border of a bubble and its text.
const BUBBLE_PAD: usize = 12;
/// Space between the top of the artwork area and the first bubble.
const BUBBLE_TOP: usize = 6;
/// Space between two bubbles in the same cell.
const BUBBLE_GAP: usize = 6;

/// One chat message that should appear in the comic.
#[derive(Debug, Clone)]
pub struct ComicMessage {
    /// Stable id of the speaker, used to look up the picture.
    pub user_id: String,
    /// Name shown in the caption box.
    pub display_name: String,
    /// Message body.
    pub text: String,
    /// Chat colour of the speaker.
    pub color: Color,
    /// Picture assigned to the speaker.
    pub avatar: Option<PathBuf>,
    /// Badges such as `moderator` or `subscriber`.
    pub badges: Vec<String>,
    /// Time of the message in milliseconds since the epoch.
    pub timestamp_ms: i64,
    /// The message is a reply to another message.
    #[allow(dead_code)]
    pub reply: bool,
}

/// A group of messages that share one panel.
///
/// The bot puts every message that arrives within the configured cell window
/// into the same cell, so several chatters can appear in one panel.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    pub first_time_ms: i64,
    pub messages: Vec<ComicMessage>,
}

/// A character of the conversation, shown in the roster of the sidebar.
#[derive(Debug, Clone)]
pub struct Character {
    pub display_name: String,
    pub color: Color,
    pub avatar: Option<PathBuf>,
    pub messages: u32,
}

/// Everything that is drawn on one comic page.
#[derive(Debug, Clone, Default)]
pub struct ComicPage {
    pub cells: Vec<Cell>,
    pub characters: Vec<Character>,
    /// PNG drawn behind the contents of every cell.
    pub background: Option<PathBuf>,
}

/// One speech bubble inside a panel.
struct Bubble {
    /// Index of the message inside the cell.
    message: usize,
    /// Index of the speaker inside `Panel::characters`.
    character: usize,
    name: String,
    color: Color,
    lines: Vec<String>,
    h: usize,
}

/// Geometry of a single panel. One panel shows one cell.
struct Panel {
    /// Index of the cell in `ComicPage::cells`.
    cell: usize,
    caption: String,
    caption_color: Color,
    badges: Vec<String>,
    bubbles: Vec<Bubble>,
    /// Message indices of the distinct speakers of the cell.
    characters: Vec<usize>,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    caption_h: usize,
    /// Horizontal position of the character inside the panel.
    character_left: bool,
}

impl Panel {
    fn bubble_height(&self) -> usize {
        if self.bubbles.is_empty() {
            return 0;
        }
        let heights: usize = self.bubbles.iter().map(|bubble| bubble.h).sum();
        heights + BUBBLE_GAP * (self.bubbles.len() - 1)
    }
}

/// Render a comic page.
pub fn render(
    page: &ComicPage,
    fonts: &Fonts,
    config: &ComicConfig,
    title: &str,
    subtitle: &str,
) -> RgbaImage {
    let geometry = Geometry::new(config);
    let panels = layout(page, fonts, config, &geometry, title);
    let height = geometry.page_height(&panels);

    let mut canvas = Canvas::new(geometry.page_w, height, Color::WHITE);

    draw_header(&mut canvas, fonts, title, subtitle, geometry.page_w);

    for panel in &panels {
        draw_panel(&mut canvas, fonts, config, &geometry, page, panel);
    }

    if geometry.sidebar_w > 0 {
        draw_sidebar(&mut canvas, fonts, config, &geometry, page, &panels);
    }

    // The border of the page.
    canvas.stroke_round_rect(
        2,
        2,
        geometry.page_w as i64 - 4,
        height as i64 - 4,
        6,
        Color::BLACK,
        4,
    );

    canvas.into_image()
}

/// Fixed measurements of the page.
struct Geometry {
    page_w: usize,
    margin: usize,
    gutter: usize,
    panel_pad: usize,
    caption_h: usize,
    columns: usize,
    rows: usize,
    sidebar_w: usize,
    panel_w: usize,
    grid_x: usize,
    grid_y: usize,
    /// How much of the character is covered by the bubble.
    overlap: usize,
    /// How much of the character stays visible below the bubble.
    visible: usize,
}

impl Geometry {
    fn new(config: &ComicConfig) -> Self {
        let page_w = config.width as usize;
        let margin = 16usize;
        let header_h = 66usize;
        let gutter = 12usize;
        let panel_pad = 10usize;
        let caption_h = 32usize;
        let columns = config.columns.max(1);
        let rows = config.rows.max(1);
        let sidebar_w = if config.show_sidebar {
            config.sidebar_width as usize
        } else {
            0
        };
        let grid_x = margin;
        let grid_w = page_w
            .saturating_sub(margin * 2)
            .saturating_sub(sidebar_w + if sidebar_w > 0 { gutter } else { 0 });
        let panel_w = (grid_w.saturating_sub(gutter * (columns - 1))) / columns;

        Self {
            page_w,
            margin,
            gutter,
            panel_pad,
            caption_h,
            columns,
            rows,
            sidebar_w,
            panel_w,
            grid_x,
            grid_y: header_h + 10,
            overlap: 30,
            visible: 175,
        }
    }

    /// Width available for the text inside a bubble.
    fn bubble_text_width(&self) -> usize {
        let bubble_pad = 12usize;
        self.panel_w
            .saturating_sub(self.panel_pad * 2)
            .saturating_sub(bubble_pad * 2)
            .max(60)
    }

    fn page_height(&self, panels: &[Panel]) -> usize {
        if panels.is_empty() {
            return self.grid_y + 80;
        }
        let bottom = panels
            .iter()
            .map(|panel| panel.y + panel.h)
            .max()
            .unwrap_or(self.grid_y);
        bottom + self.margin
    }
}

/// Compute the position and size of every panel.
fn layout(
    page: &ComicPage,
    fonts: &Fonts,
    config: &ComicConfig,
    geometry: &Geometry,
    title: &str,
) -> Vec<Panel> {
    let body_metrics = fonts.regular.metrics(config.font_size);
    let name_size = (config.name_font_size * 0.78).round();
    let name_metrics = fonts.bold.metrics(name_size);
    let text_width = geometry.bubble_text_width();
    let name_h = name_metrics.line_height.ceil() as usize;

    let max_cells = (geometry.columns * geometry.rows).max(1);
    let start = page.cells.len().saturating_sub(max_cells);
    let mut panels: Vec<Panel> = Vec::with_capacity(page.cells.len() - start);

    // The newest cell is shown first, so the conversation reads from the
    // latest panel back into the history.
    for (offset, cell_index) in (start..page.cells.len()).rev().enumerate() {
        let cell = &page.cells[cell_index];
        let mut bubbles = Vec::with_capacity(cell.messages.len());
        let mut character_messages: Vec<usize> = Vec::new();
        let mut character_keys: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();

        for (message_index, message) in cell.messages.iter().enumerate() {
            let key = if message.user_id.is_empty() {
                message.display_name.to_lowercase()
            } else {
                message.user_id.clone()
            };
            let character = match character_keys.iter().position(|known| known == &key) {
                Some(index) => index,
                None => {
                    if !names
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&message.display_name))
                    {
                        names.push(message.display_name.clone());
                    }
                    character_keys.push(key);
                    character_messages.push(message_index);
                    character_keys.len() - 1
                }
            };

            let lines = wrap_text(&fonts.regular, config.font_size, &message.text, text_width);
            let text_h = lines.len() * body_metrics.line_height.ceil() as usize;
            let bubble_h = BUBBLE_PAD * 2 + name_h + text_h;

            bubbles.push(Bubble {
                message: message_index,
                character,
                name: message.display_name.to_uppercase(),
                color: message.color,
                lines,
                h: bubble_h,
            });
        }

        if bubbles.is_empty() {
            continue;
        }

        let first = &cell.messages[bubbles[0].message];
        let caption = if offset == 0 {
            format!("MEANWHILE IN {}", title.to_uppercase())
        } else {
            names.join(", ").to_uppercase()
        };

        panels.push(Panel {
            cell: cell_index,
            caption,
            caption_color: first.color,
            badges: first.badges.clone(),
            bubbles,
            characters: character_messages,
            x: 0,
            y: 0,
            w: geometry.panel_w,
            h: 0,
            caption_h: geometry.caption_h,
            character_left: offset % 2 == 0,
        });
    }

    // Panels of one row share the same height.
    let mut y = geometry.grid_y;
    let mut index = 0;
    while index < panels.len() {
        let row_end = (index + geometry.columns).min(panels.len());
        let art_h = panels[index..row_end]
            .iter()
            .map(|panel| {
                panel.bubble_height() + BUBBLE_TOP + geometry.overlap + geometry.visible
            })
            .max()
            .unwrap_or(geometry.visible);
        for (column, panel) in panels[index..row_end].iter_mut().enumerate() {
            panel.x = geometry.grid_x + column * (geometry.panel_w + geometry.gutter);
            panel.y = y;
            panel.h = geometry.caption_h + art_h;
        }
        y += geometry.caption_h + art_h + geometry.gutter;
        index = row_end;
    }

    panels
}

/// Draw one panel: border, caption box, characters and speech bubbles.
fn draw_panel(
    canvas: &mut Canvas,
    fonts: &Fonts,
    config: &ComicConfig,
    geometry: &Geometry,
    page: &ComicPage,
    panel: &Panel,
) {
    let cell = &page.cells[panel.cell.min(page.cells.len().saturating_sub(1))];
    let px = panel.x as i64;
    let py = panel.y as i64;
    let pw = panel.w as i64;
    let ph = panel.h as i64;
    let pad = geometry.panel_pad as i64;

    // Panel background. The frame is drawn last, it crops the characters.
    canvas.fill_rect(px, py, pw, ph, Color::WHITE);
    if let Some(background) = load_background(page.background.as_ref(), pw as u32, ph as u32) {
        canvas.draw_image(&background, px, py);
    }

    let art_top = py + panel.caption_h as i64;
    let art_bottom = py + ph;
    let inner_w = pw - pad * 2;
    let bubble_total = panel.bubble_height() as i64;
    // The heads of the characters hide behind the bubbles, the rest is visible.
    let character_h = (art_bottom
        - art_top
        - BUBBLE_TOP as i64
        - bubble_total
        + geometry.overlap as i64)
        .max(40);
    let character_y = art_bottom - character_h;

    let count = panel.characters.len().max(1) as i64;
    let step = inner_w / count;
    let character_w = if count == 1 {
        inner_w * 78 / 100
    } else {
        step + step / 6
    };
    let mut centers: Vec<i64> = Vec::with_capacity(count as usize);

    canvas.set_clip(Some((px + 2, art_top, px + pw - 2, art_bottom)));
    for (index, message_index) in panel.characters.iter().enumerate() {
        let message_index = (*message_index).min(cell.messages.len().saturating_sub(1));
        let message = &cell.messages[message_index];
        let character_x = if count == 1 {
            if panel.character_left {
                px + pad
            } else {
                px + pw - pad - character_w
            }
        } else {
            let slot = px + pad + index as i64 * step;
            slot + (step - character_w) / 2
        };
        centers.push(character_x + character_w / 2);

        match load_avatar(message.avatar.as_ref(), character_w as u32, character_h as u32) {
            Some(image) => {
                let x = character_x + (character_w - image.width() as i64) / 2;
                let y = character_y + (character_h - image.height() as i64);
                canvas.draw_image(&image, x, y);
            }
            None => {
                // Placeholder with the initials of the speaker.
                canvas.fill_round_rect(
                    character_x,
                    character_y,
                    character_w,
                    character_h,
                    10,
                    Color::rgb(228, 224, 216),
                );
                let initials = initials(&message.display_name);
                let size = (character_w as f32 * 0.42).min(character_h as f32 * 0.32);
                let width = fonts.bold.measure(size, &initials);
                fonts.bold.draw(
                    canvas,
                    size,
                    character_x as f32 + (character_w as f32 - width) / 2.0,
                    character_y as f32 + character_h as f32 * 0.62,
                    &initials,
                    Color::rgb(120, 115, 110),
                );
            }
        }
    }
    if centers.is_empty() {
        centers.push(px + pw / 2);
    }
    canvas.set_clip(None);

    // Speech bubbles: they sit on top of the characters.
    let bubble_x = px + pad;
    let bubble_w = pw - pad * 2;
    let radius = 14i64;
    let name_size = (config.name_font_size * 0.78).round();
    let name_metrics = fonts.bold.metrics(name_size);
    let name_h = name_metrics.line_height.ceil() as i64;
    let body_metrics = fonts.regular.metrics(config.font_size);
    let mut bubble_y = art_top + BUBBLE_TOP as i64;

    for bubble in &panel.bubbles {
        let bubble_h = bubble.h as i64;
        let center_x = centers
            .get(bubble.character)
            .copied()
            .unwrap_or(px + pw / 2);

        canvas.fill_round_rect(
            bubble_x + 3,
            bubble_y + 4,
            bubble_w,
            bubble_h,
            radius,
            Color::rgba(0, 0, 0, 26),
        );
        canvas.fill_round_rect(bubble_x, bubble_y, bubble_w, bubble_h, radius, Color::WHITE);
        canvas.stroke_round_rect(bubble_x, bubble_y, bubble_w, bubble_h, radius, Color::BLACK, 3);

        // The tail points at the speaker.
        let tail_y = bubble_y + bubble_h;
        canvas.fill_triangle(
            (center_x - 13, tail_y - 2),
            (center_x + 13, tail_y - 2),
            (center_x, tail_y + 24),
            Color::WHITE,
        );
        canvas.fill_triangle(
            (center_x - 13, tail_y - 3),
            (center_x + 13, tail_y - 3),
            (center_x, tail_y + 24),
            Color::BLACK,
        );
        canvas.fill_triangle(
            (center_x - 10, tail_y - 2),
            (center_x + 10, tail_y - 2),
            (center_x, tail_y + 21),
            Color::WHITE,
        );

        // The speaker name is printed inside the top of the bubble.
        let text_x = (bubble_x + BUBBLE_PAD as i64) as f32;
        fonts.bold.draw(
            canvas,
            name_size,
            text_x,
            bubble_y as f32 + BUBBLE_PAD as f32 + name_metrics.ascent,
            &bubble.name,
            bubble.color,
        );

        let mut baseline =
            bubble_y as f32 + BUBBLE_PAD as f32 + name_h as f32 + body_metrics.ascent;
        for line in &bubble.lines {
            fonts.regular.draw(
                canvas,
                config.font_size,
                text_x,
                baseline,
                line,
                Color::rgb(26, 24, 22),
            );
            baseline += body_metrics.line_height;
        }

        bubble_y += bubble_h + BUBBLE_GAP as i64;
    }

    // Caption box in the top left corner of the panel.
    draw_caption(canvas, fonts, config, panel, px, py);

    // Panel frame on top of everything.
    canvas.stroke_rect(px, py, pw, ph, Color::BLACK, 3);
}

/// Draw the caption box of a panel.
fn draw_caption(
    canvas: &mut Canvas,
    fonts: &Fonts,
    config: &ComicConfig,
    panel: &Panel,
    px: i64,
    py: i64,
) {
    let size = (config.name_font_size * 0.82).round();
    let metrics = fonts.bold.metrics(size);
    let max_text_width = (panel.w as i64 - 12 - 8 * 2 - 8) as f32;
    let caption = elide(&fonts.bold, size, &panel.caption, max_text_width);
    let text_width = fonts.bold.measure(size, &caption);
    let badge_labels: Vec<&str> = panel
        .badges
        .iter()
        .filter_map(|badge| badge_style(badge).map(|(label, _)| label))
        .collect();
    let badge_width: f32 = badge_labels
        .iter()
        .map(|label| fonts.bold.measure(size * 0.72, label) + 16.0)
        .sum::<f32>();

    let pad = 8i64;
    let box_w = (text_width + badge_width + pad as f32 * 2.0 + 8.0) as i64;
    let box_w = box_w.min(panel.w as i64 - 12);
    let box_h = (metrics.ascent + metrics.descent) as i64 + 10;
    let box_x = px + 8;
    let box_y = py + 8;

    canvas.fill_rect(box_x + 3, box_y + 3, box_w, box_h, Color::rgba(0, 0, 0, 40));
    canvas.fill_rect(box_x, box_y, box_w, box_h, Color::WHITE);
    canvas.stroke_rect(box_x, box_y, box_w, box_h, Color::BLACK, 2);

    let baseline = box_y as f32 + 5.0 + metrics.ascent;
    fonts.bold.draw(
        canvas,
        size,
        (box_x + pad) as f32,
        baseline,
        &caption,
        panel.caption_color,
    );
}

/// Draw the roster on the right side of the page.
fn draw_sidebar(
    canvas: &mut Canvas,
    fonts: &Fonts,
    config: &ComicConfig,
    geometry: &Geometry,
    page: &ComicPage,
    panels: &[Panel],
) {
    let x = (geometry.page_w - geometry.margin - geometry.sidebar_w) as i64;
    let y = geometry.grid_y as i64;
    let w = geometry.sidebar_w as i64;
    let bottom = panels
        .iter()
        .map(|panel| panel.y + panel.h)
        .max()
        .unwrap_or(geometry.grid_y) as i64;
    let h = (bottom - y).max(120);

    canvas.fill_rect(x, y, w, h, Color::WHITE);
    canvas.stroke_rect(x, y, w, h, Color::BLACK, 3);

    let title_size = 18.0f32;
    let title_metrics = fonts.bold.metrics(title_size);
    let title = "CHARACTERS";
    canvas.fill_rect(x + 8, y + 8, w - 16, (title_metrics.ascent + title_metrics.descent) as i64 + 10, Color::WHITE);
    canvas.stroke_rect(x + 8, y + 8, w - 16, (title_metrics.ascent + title_metrics.descent) as i64 + 10, Color::BLACK, 2);
    fonts.bold.draw(
        canvas,
        title_size,
        (x + 16) as f32,
        y as f32 + 12.0 + title_metrics.ascent,
        title,
        Color::BLACK,
    );

    // Roster: a small picture and the name of every chatter.
    let thumb = 44i64;
    let name_size = 14.0f32;
    let name_metrics = fonts.regular.metrics(name_size);
    let entry_h = thumb + 10;
    let mut entry_y = y + 8 + (title_metrics.ascent + title_metrics.descent) as i64 + 18;
    let max_entries = ((h - (entry_y - y) - 150) / entry_h).max(0) as usize;

    for (index, character) in page.characters.iter().enumerate() {
        if index >= max_entries {
            break;
        }
        let thumb_x = x + 12;
        canvas.set_clip(Some((thumb_x, entry_y, thumb_x + thumb, entry_y + thumb)));
        match load_avatar(character.avatar.as_ref(), thumb as u32, thumb as u32) {
            Some(image) => canvas.draw_image(&image, thumb_x, entry_y),
            None => {
                canvas.fill_round_rect(thumb_x, entry_y, thumb, thumb, 8, Color::rgb(226, 222, 214));
                let initials = initials(&character.display_name);
                let size = 20.0f32;
                let width = fonts.bold.measure(size, &initials);
                fonts.bold.draw(
                    canvas,
                    size,
                    thumb_x as f32 + (thumb as f32 - width) / 2.0,
                    entry_y as f32 + thumb as f32 * 0.66,
                    &initials,
                    Color::rgb(120, 115, 110),
                );
            }
        }
        canvas.set_clip(None);
        canvas.stroke_round_rect(thumb_x, entry_y, thumb, thumb, 8, Color::BLACK, 2);

        let name_x = thumb_x + thumb + 10;
        let available = (x + w - 12 - name_x) as f32;
        let name = elide(&fonts.regular, name_size, &character.display_name, available);
        fonts.regular.draw(
            canvas,
            name_size,
            name_x as f32,
            entry_y as f32 + thumb as f32 * 0.45,
            &name,
            character.color,
        );
        fonts.regular.draw(
            canvas,
            name_size * 0.85,
            name_x as f32,
            entry_y as f32 + thumb as f32 * 0.45 + name_metrics.line_height * 0.9,
            &format!("{} message{}", character.messages, if character.messages == 1 { "" } else { "s" }),
            Color::rgb(140, 135, 128),
        );
        entry_y += entry_h;
    }

    // Big picture of the character that spoke last.
    let last = page.cells.iter().rev().find_map(|cell| cell.messages.last());
    if let Some(message) = last {
        let box_h = (y + h - 12 - entry_y).max(60);
        if box_h > 70 {
            let box_x = x + 12;
            let box_y = entry_y + 8;
            canvas.fill_rect(box_x, box_y, w - 24, box_h, Color::rgb(246, 243, 235));
            canvas.stroke_rect(box_x, box_y, w - 24, box_h, Color::BLACK, 2);
            canvas.set_clip(Some((box_x + 2, box_y + 2, box_x + w - 24 - 2, box_y + box_h - 2)));
            if let Some(image) =
                load_avatar(message.avatar.as_ref(), (w - 40) as u32, (box_h - 20) as u32)
            {
                let ix = box_x + ((w - 24) - image.width() as i64) / 2;
                let iy = box_y + ((box_h) - image.height() as i64);
                canvas.draw_image(&image, ix, iy);
            }
            canvas.set_clip(None);
        }
    }
    let _ = config;
}

/// Draw the title bar of the page.
fn draw_header(canvas: &mut Canvas, fonts: &Fonts, title: &str, subtitle: &str, width: usize) {
    let comic = fonts.comic.as_ref().unwrap_or(&fonts.bold);
    let size = 30.0f32;
    let metrics = comic.metrics(size);
    let margin = 18usize;

    canvas.fill_rect(0, 0, width as i64, 62, Color::rgb(240, 237, 228));
    canvas.fill_rect(0, 62, width as i64, 4, Color::BLACK);
    comic.draw(
        canvas,
        size,
        margin as f32,
        20.0 + metrics.ascent,
        title,
        Color::BLACK,
    );
    let title_w = comic.measure(size, title);
    fonts.regular.draw(
        canvas,
        17.0,
        margin as f32 + title_w + 14.0,
        20.0 + metrics.ascent,
        subtitle,
        Color::rgb(110, 100, 90),
    );
}

/// Shorten a text so it fits into `max_width`.
fn elide(font: &Font, size: f32, text: &str, max_width: f32) -> String {
    if font.measure(size, text) <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    for ch in text.chars() {
        if font.measure(size, &format!("{out}{ch}...")) > max_width {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

/// Load an avatar PNG and scale it so that it covers the given box.
///
/// The picture may become wider than the box; the panel frame crops it, the
/// same way a comic panel cuts off a figure.
fn load_avatar(path: Option<&PathBuf>, max_w: u32, max_h: u32) -> Option<RgbaImage> {
    let path = path?;
    let reader = image::ImageReader::open(path).ok()?;
    let decoded = reader.decode().ok()?;
    let rgba = decoded.to_rgba8();
    if max_w == 0 || max_h == 0 || rgba.width() == 0 || rgba.height() == 0 {
        return None;
    }
    Some(fit_rgba(&rgba, max_w, max_h))
}

/// Load a background PNG and scale it so that it covers the panel.
///
/// Unlike avatars, backgrounds fill the whole cell. The picture is scaled up
/// until both dimensions are covered, then the centre is cropped to fit.
fn load_background(path: Option<&PathBuf>, width: u32, height: u32) -> Option<RgbaImage> {
    let path = path?;
    let reader = image::ImageReader::open(path).ok()?;
    let decoded = reader.decode().ok()?;
    let rgba = decoded.to_rgba8();
    if width == 0 || height == 0 || rgba.width() == 0 || rgba.height() == 0 {
        return None;
    }
    let scale = (width as f32 / rgba.width() as f32)
        .max(height as f32 / rgba.height() as f32);
    let scaled_w = ((rgba.width() as f32 * scale).round() as u32).max(width);
    let scaled_h = ((rgba.height() as f32 * scale).round() as u32).max(height);
    let resized = image::imageops::resize(
        &rgba,
        scaled_w,
        scaled_h,
        image::imageops::FilterType::Triangle,
    );
    let x = (scaled_w - width) / 2;
    let y = (scaled_h - height) / 2;
    Some(image::imageops::crop_imm(&resized, x, y, width, height).to_image())
}

/// Label and colour for a chat badge.
fn badge_style(badge: &str) -> Option<(&'static str, Color)> {
    match badge {
        "broadcaster" => Some(("OWNER", Color::rgb(230, 0, 0))),
        "moderator" => Some(("MOD", Color::rgb(0, 173, 3))),
        "vip" => Some(("VIP", Color::rgb(226, 62, 140))),
        "subscriber" => Some(("SUB", Color::rgb(145, 70, 255))),
        "founder" => Some(("FOUND", Color::rgb(224, 177, 0))),
        _ => None,
    }
}

/// Initials of a name, used by the placeholder picture.
fn initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    let mut out = String::new();
    for part in parts.iter().take(2) {
        if let Some(first) = part.chars().next() {
            out.push(first.to_ascii_uppercase());
        }
    }
    if out.is_empty() {
        "?".to_string()
    } else {
        out
    }
}

/// Split a message into lines that fit into `max_width` pixels.
pub fn wrap_text(font: &Font, size: f32, text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.trim().is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if font.measure(size, &candidate) <= max_width as f32 || current.is_empty() {
                current = candidate;
            } else {
                lines.push(std::mem::take(&mut current));
                current = word.to_string();
            }
            // A single word that is wider than the bubble gets hard split.
            while font.measure(size, &current) > max_width as f32 && current.chars().count() > 1 {
                let mut split = current.len();
                while split > 1 {
                    let candidate = &current[..split];
                    if !candidate.is_char_boundary(split) {
                        split -= 1;
                        continue;
                    }
                    if font.measure(size, candidate) <= max_width as f32 {
                        break;
                    }
                    split -= 1;
                }
                let (head, tail) = current.split_at(split);
                lines.push(head.to_string());
                current = tail.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_respects_width() {
        let data = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf").unwrap();
        let font = Font::from_bytes(data).unwrap();
        let text = "the quick brown fox jumps over the lazy dog again and again and again";
        let lines = wrap_text(&font, 20.0, text, 200);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(font.measure(20.0, line) <= 200.0 + 1.0, "line too wide: {line}");
        }
    }

    #[test]
    fn wrapping_keeps_long_words() {
        let data = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf").unwrap();
        let font = Font::from_bytes(data).unwrap();
        let long = "x".repeat(400);
        let lines = wrap_text(&font, 20.0, &long, 120);
        assert!(lines.len() >= 3);
    }

    #[test]
    fn panels_are_arranged_in_a_grid() {
        let config = ComicConfig::default();
        let geometry = Geometry::new(&config);
        assert_eq!(geometry.columns, 3);
        assert_eq!(geometry.rows, 2);
        assert!(geometry.panel_w > 100);
        assert!(geometry.sidebar_w > 0);
    }

    #[test]
    fn layout_limits_the_page_to_columns_times_rows() {
        let config = ComicConfig::default();
        let geometry = Geometry::new(&config);
        let fonts = Fonts::load(&crate::config::FontConfig::default()).unwrap();
        let cells: Vec<Cell> = (0..8)
            .map(|index| Cell {
                first_time_ms: index * 2000,
                messages: vec![ComicMessage {
                    user_id: "tester".to_string(),
                    display_name: "Tester".to_string(),
                    text: "hello".to_string(),
                    color: Color::rgb(0, 173, 255),
                    avatar: None,
                    badges: Vec::new(),
                    timestamp_ms: index * 2000,
                    reply: false,
                }],
            })
            .collect();
        let page = ComicPage {
            cells,
            characters: Vec::new(),
            background: None,
        };
        let panels = layout(&page, &fonts, &config, &geometry, "#test");
        assert_eq!(panels.len(), 6);
        assert_eq!(panels[0].cell, 7, "the newest cell is displayed first");
        assert_eq!(panels[5].cell, 2, "the oldest cells are left out");
    }

    #[test]
    fn background_covers_the_panel() {
        let dir = std::env::temp_dir().join(format!(
            "comicbot-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bg.png");
        RgbaImage::from_pixel(10, 10, image::Rgba([200, 20, 30, 255]))
            .save(&path)
            .unwrap();

        let loaded = load_background(Some(&path), 100, 50).expect("background loads");
        assert_eq!(loaded.dimensions(), (100, 50));
        assert_eq!(
            loaded.get_pixel(0, 0),
            &image::Rgba([200, 20, 30, 255])
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn multi_speaker_cell_renders() {
        let config = ComicConfig::default();
        let fonts = Fonts::load(&crate::config::FontConfig::default()).unwrap();
        let message = |user_id: &str, name: &str, text: &str, color: Color| ComicMessage {
            user_id: user_id.to_string(),
            display_name: name.to_string(),
            text: text.to_string(),
            color,
            avatar: None,
            badges: Vec::new(),
            timestamp_ms: 0,
            reply: false,
        };
        let page = ComicPage {
            cells: vec![Cell {
                first_time_ms: 0,
                messages: vec![
                    message("alice", "Alice", "hello there", Color::rgb(0, 173, 255)),
                    message("bob", "Bob", "hi Alice", Color::rgb(214, 66, 87)),
                    message("alice", "Alice", "how are you?", Color::rgb(0, 173, 255)),
                ],
            }],
            characters: Vec::new(),
            background: None,
        };
        let image = render(&page, &fonts, &config, "#test", "two speakers");
        assert!(image.width() > 100);
        assert!(image.height() > 100);
    }
}
