//! Native window that shows the newest comic.
//!
//! The window lives on its own thread. The bot sends a rendered page through a
//! channel; the thread wakes up immediately and replaces the window contents,
//! so there is no polling delay like a browser page would have.

use crate::render::fit_rgba;
use image::RgbaImage;
use minifb::{Window, WindowOptions};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;

/// Handle to the comic window.
pub struct ComicDisplay {
    sender: Mutex<mpsc::Sender<RgbaImage>>,
}

impl ComicDisplay {
    /// Open the window on a dedicated thread.
    ///
    /// Returns `None` when no display server is available. The bot keeps
    /// running in that case and still writes the rendered PNG files.
    ///
    /// The returned receiver resolves when the window is closed or its render
    /// thread stops, so the application can shut down with it.
    pub fn spawn(width: usize) -> Option<(Self, oneshot::Receiver<()>)> {
        let width = width.max(320);
        let height = (width * 2 / 3).max(240);
        let (sender, receiver) = mpsc::channel::<RgbaImage>();
        let (ready_sender, ready_receiver) = mpsc::channel::<bool>();
        let (closed_sender, closed_receiver) = oneshot::channel::<()>();

        std::thread::Builder::new()
            .name("comic-window".to_string())
            .spawn(move || {
                let mut window = match Window::new(
                    "comicbot",
                    width,
                    height,
                    WindowOptions {
                        resize: false,
                        ..WindowOptions::default()
                    },
                ) {
                    Ok(window) => window,
                    Err(err) => {
                        eprintln!("display: could not open a window: {err}");
                        let _ = ready_sender.send(false);
                        let _ = closed_sender.send(());
                        return;
                    }
                };
                // The bars around a shorter page blend with the paper.
                window.set_background_color(255, 255, 255);
                window.set_target_fps(30);
                let _ = ready_sender.send(true);

                let mut buffer = vec![0x00FF_FFFFu32; width * height];
                loop {
                    if !window.is_open() {
                        break;
                    }
                    match receiver.recv_timeout(Duration::from_millis(16)) {
                        Ok(image) => {
                            // Scale the page ourselves. minifb's aspect-ratio
                            // scaler has an out-of-bounds path for pages that
                            // are taller than the window.
                            let scaled = fit_rgba(&image, width as u32, height as u32);
                            let x0 = (width.saturating_sub(scaled.width() as usize)) / 2;
                            let y0 = (height.saturating_sub(scaled.height() as usize)) / 2;
                            buffer.fill(0x00FF_FFFF);
                            for (sx, sy, pixel) in scaled.enumerate_pixels() {
                                let x = x0 + sx as usize;
                                let y = y0 + sy as usize;
                                if x < width && y < height {
                                    buffer[y * width + x] = ((pixel[0] as u32) << 16)
                                        | ((pixel[1] as u32) << 8)
                                        | pixel[2] as u32;
                                }
                            }
                            if window
                                .update_with_buffer(&buffer, width, height)
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            // Pump window events (including a close request)
                            // without uploading the same frame again.
                            window.update();
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                let _ = closed_sender.send(());
            })
            .ok()?;

        match ready_receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(true) => Some((
                Self {
                    sender: Mutex::new(sender),
                },
                closed_receiver,
            )),
            _ => None,
        }
    }

    /// Show a rendered page in the window.
    pub fn show(&self, image: RgbaImage) {
        if let Ok(sender) = self.sender.lock() {
            let _ = sender.send(image);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test for the window backend; run with `--ignored` on a machine
    /// that has a display server.
    #[test]
    #[ignore = "requires a display server"]
    fn window_opens_and_accepts_a_frame() {
        let (display, _closed) = ComicDisplay::spawn(320).expect("a window is available");
        // A tall page used to hit an out-of-bounds path in minifb's scaler.
        display.show(RgbaImage::new(320, 900));
        std::thread::sleep(Duration::from_millis(500));
    }
}
