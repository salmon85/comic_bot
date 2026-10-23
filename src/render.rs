//! A tiny software rasterizer.
//!
//! Everything the comic renderer needs is drawn by hand into an RGBA8 buffer:
//! rounded rectangles, triangles, images and anti-aliased text. Keeping this
//! dependency-free means the whole program stays a single static binary.

use image::RgbaImage;

/// RGBA color with straight (non premultiplied) alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const BLACK: Color = Color::rgb(20, 18, 16);
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);

    /// Parse `#rrggbb` / `#rrggbbaa`; returns `None` for anything else.
    pub fn parse_hex(text: &str) -> Option<Color> {
        let text = text.trim().trim_start_matches('#');
        let bytes: Vec<u8> = text
            .as_bytes()
            .chunks(2)
            .filter_map(|c| {
                let s = std::str::from_utf8(c).ok()?;
                u8::from_str_radix(s, 16).ok()
            })
            .collect();
        match bytes.len() {
            3 => Some(Color::rgb(bytes[0], bytes[1], bytes[2])),
            4 => Some(Color::rgba(bytes[0], bytes[1], bytes[2], bytes[3])),
            _ => None,
        }
    }
}

/// An RGBA image buffer that can be drawn into.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    data: Vec<u8>,
    /// Optional clip rectangle (x0, y0, x1, y1) for drawing.
    clip: Option<(i64, i64, i64, i64)>,
}

impl Canvas {
    pub fn new(width: usize, height: usize, background: Color) -> Self {
        let mut data = Vec::with_capacity(width * height * 4);
        for _ in 0..(width * height) {
            data.push(background.r);
            data.push(background.g);
            data.push(background.b);
            data.push(background.a);
        }
        Self {
            width,
            height,
            data,
            clip: None,
        }
    }

    /// Limit all following drawing to a rectangle. `None` removes the limit.
    pub fn set_clip(&mut self, rect: Option<(i64, i64, i64, i64)>) {
        self.clip = rect;
    }

    #[inline]
    fn blend(&mut self, x: i64, y: i64, color: Color) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return;
        }
        if let Some((x0, y0, x1, y1)) = self.clip {
            if x < x0 || y < y0 || x >= x1 || y >= y1 {
                return;
            }
        }
        let idx = (y as usize * self.width + x as usize) * 4;
        let alpha = color.a as u32;
        if alpha == 0 {
            return;
        }
        if alpha == 255 {
            self.data[idx] = color.r;
            self.data[idx + 1] = color.g;
            self.data[idx + 2] = color.b;
            self.data[idx + 3] = 255;
            return;
        }
        let dst_a = self.data[idx + 3] as u32;
        let out_a = alpha + dst_a * (255 - alpha) / 255;
        if out_a == 0 {
            self.data[idx + 3] = 0;
            return;
        }
        for c in 0..3 {
            let src = [color.r, color.g, color.b][c] as u32 * alpha;
            let dst = self.data[idx + c] as u32 * dst_a * (255 - alpha) / 255;
            self.data[idx + c] = (((src + dst) / 255) as u32).min(255) as u8;
        }
        self.data[idx + 3] = out_a.min(255) as u8;
    }

    /// Draw a filled rectangle.
    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, color: Color) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, color);
            }
        }
    }

    /// Draw a rectangle outline.
    pub fn stroke_rect(&mut self, x: i64, y: i64, w: i64, h: i64, color: Color, thickness: i64) {
        for t in 0..thickness {
            self.fill_rect(x + t, y + t, w - 2 * t, 1, color);
            self.fill_rect(x + t, y + h - 1 - t, w - 2 * t, 1, color);
            self.fill_rect(x + t, y + t, 1, h - 2 * t, color);
            self.fill_rect(x + w - 1 - t, y + t, 1, h - 2 * t, color);
        }
    }

    /// Fill a rounded rectangle.
    pub fn fill_round_rect(&mut self, x: i64, y: i64, w: i64, h: i64, radius: i64, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        let r = radius.min(w / 2).min(h / 2).max(0);
        for yy in y..y + h {
            for xx in x..x + w {
                if inside_round_rect(xx as f64, yy as f64, x, y, w, h, r) {
                    self.blend(xx, yy, color);
                }
            }
        }
    }

    /// Stroke a rounded rectangle.
    pub fn stroke_round_rect(
        &mut self,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
        radius: i64,
        color: Color,
        thickness: i64,
    ) {
        if w <= 0 || h <= 0 || thickness <= 0 {
            return;
        }
        let r = radius.min(w / 2).min(h / 2).max(0);
        let t = thickness.min(w / 2).min(h / 2);
        for yy in y..y + h {
            for xx in x..x + w {
                let outside = inside_round_rect(xx as f64, yy as f64, x, y, w, h, r);
                let inside = inside_round_rect(
                    xx as f64,
                    yy as f64,
                    x + t,
                    y + t,
                    w - 2 * t,
                    h - 2 * t,
                    (r - t).max(0),
                );
                if outside && !inside {
                    self.blend(xx, yy, color);
                }
            }
        }
    }

    /// Fill a triangle given by three vertices.
    pub fn fill_triangle(&mut self, a: (i64, i64), b: (i64, i64), c: (i64, i64), color: Color) {
        let min_x = a.0.min(b.0).min(c.0);
        let max_x = a.0.max(b.0).max(c.0);
        let min_y = a.1.min(b.1).min(c.1);
        let max_y = a.1.max(b.1).max(c.1);
        let area = cross(a, b, c);
        if area == 0 {
            return;
        }
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let p = (x, y);
                let w0 = cross(b, c, p) as f64 / area as f64;
                let w1 = cross(c, a, p) as f64 / area as f64;
                let w2 = cross(a, b, p) as f64 / area as f64;
                if w0 >= -1e-9 && w1 >= -1e-9 && w2 >= -1e-9 {
                    self.blend(x, y, color);
                }
            }
        }
    }

    /// Alpha blend another image at the given position.
    pub fn draw_image(&mut self, image: &RgbaImage, x: i64, y: i64) {
        for (sx, sy, pixel) in image.enumerate_pixels() {
            self.blend(x + sx as i64, y + sy as i64, Color::rgba(pixel[0], pixel[1], pixel[2], pixel[3]));
        }
    }

    /// Convert the canvas into an image buffer.
    pub fn into_image(self) -> RgbaImage {
        RgbaImage::from_raw(self.width as u32, self.height as u32, self.data)
            .expect("canvas buffer has the right size")
    }
}

fn cross(a: (i64, i64), b: (i64, i64), c: (i64, i64)) -> i64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

fn inside_round_rect(px: f64, py: f64, x: i64, y: i64, w: i64, h: i64, radius: i64) -> bool {
    let x0 = x as f64;
    let y0 = y as f64;
    let x1 = (x + w) as f64;
    let y1 = (y + h) as f64;
    if px < x0 || py < y0 || px >= x1 || py >= y1 {
        return false;
    }
    if radius <= 0 {
        return true;
    }
    let r = radius as f64;
    let cx = if px < x0 + r {
        x0 + r
    } else if px > x1 - r {
        x1 - r
    } else {
        px
    };
    let cy = if py < y0 + r {
        y0 + r
    } else if py > y1 - r {
        y1 - r
    } else {
        py
    };
    let dx = px - cx;
    let dy = py - cy;
    dx * dx + dy * dy <= r * r
}

/// Vertical metrics of a font at a given size, in pixels.
#[derive(Debug, Clone, Copy)]
pub struct TextMetrics {
    /// Distance from the baseline to the top of the tallest glyph.
    pub ascent: f32,
    /// Distance from the baseline to the bottom of the lowest glyph.
    pub descent: f32,
    /// Recommended distance between two baselines.
    pub line_height: f32,
}

/// A loaded font plus helpers for measuring and drawing text.
pub struct Font {
    inner: fontdue::Font,
}

impl Font {
    pub fn from_bytes(data: Vec<u8>) -> anyhow::Result<Self> {
        let inner = fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
            .map_err(|e| anyhow::anyhow!("failed to load font: {e}"))?;
        Ok(Self { inner })
    }

    /// Vertical metrics of the font at the given size.
    ///
    /// Falls back to a rough estimate for fonts without line metrics.
    pub fn metrics(&self, size: f32) -> TextMetrics {
        match self.inner.horizontal_line_metrics(size) {
            Some(metrics) => TextMetrics {
                ascent: metrics.ascent,
                descent: -metrics.descent,
                line_height: metrics.new_line_size,
            },
            None => TextMetrics {
                ascent: size * 0.8,
                descent: size * 0.2,
                line_height: size * 1.2,
            },
        }
    }

    /// Width of `text` in pixels when rendered at `size`.
    pub fn measure(&self, size: f32, text: &str) -> f32 {
        let mut width = 0.0;
        for ch in text.chars() {
            let (metrics, _) = self.inner.rasterize(ch, size);
            width += metrics.advance_width;
        }
        width
    }

    /// Draw `text` with its baseline at `baseline_y`, returns the advance width.
    pub fn draw(
        &self,
        canvas: &mut Canvas,
        size: f32,
        x: f32,
        baseline_y: f32,
        text: &str,
        color: Color,
    ) -> f32 {
        let mut pen = x;
        for ch in text.chars() {
            let (metrics, bitmap) = self.inner.rasterize(ch, size);
            // The bitmap starts at the top of the glyph; its bottom edge is
            // `baseline - ymin` (ymin is negative for descenders).
            let dx = pen + metrics.xmin as f32;
            let dy = baseline_y - metrics.ymin as f32 - metrics.height as f32;
            for row in 0..metrics.height {
                for col in 0..metrics.width {
                    let coverage = bitmap[row * metrics.width + col];
                    if coverage == 0 {
                        continue;
                    }
                    let alpha = (color.a as u32 * coverage as u32 / 255) as u8;
                    canvas.blend(
                        dx as i64 + col as i64,
                        dy as i64 + row as i64,
                        Color::rgba(color.r, color.g, color.b, alpha),
                    );
                }
            }
            pen += metrics.advance_width;
        }
        pen - x
    }
}

/// Resize an image so that it fits into `max_w` x `max_h` while keeping the
/// aspect ratio. The result is returned in RGBA.
pub fn fit_rgba(image: &RgbaImage, max_w: u32, max_h: u32) -> RgbaImage {
    if image.width() == 0 || image.height() == 0 {
        return RgbaImage::new(max_w, max_h);
    }
    let scale = (max_w as f32 / image.width() as f32).min(max_h as f32 / image.height() as f32);
    let w = ((image.width() as f32 * scale).round() as u32).max(1);
    let h = ((image.height() as f32 * scale).round() as u32).max(1);
    image::imageops::resize(image, w, h, image::imageops::FilterType::Triangle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_rect_corners_are_clipped() {
        let mut canvas = Canvas::new(20, 20, Color::TRANSPARENT);
        canvas.fill_round_rect(0, 0, 20, 20, 6, Color::WHITE);
        let image = canvas.into_image();
        assert_eq!(image.get_pixel(0, 0)[3], 0, "corner should be transparent");
        assert_eq!(image.get_pixel(10, 10)[3], 255, "center should be opaque");
    }

    #[test]
    fn hex_colors_parse() {
        assert_eq!(Color::parse_hex("#ff0000"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::parse_hex("#00ff0080"), Some(Color::rgba(0, 255, 0, 128)));
        assert_eq!(Color::parse_hex("nope"), None);
    }

    #[test]
    fn text_has_positive_width() {
        let data = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf").unwrap();
        let font = Font::from_bytes(data).unwrap();
        assert!(font.measure(20.0, "hello") > 0.0);
    }
}
