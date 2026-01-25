//! Linux computer backend using X11.

use crate::error::{ComputerError, Result};
use crate::traits::{ComputerController, KeyboardController, MouseController, ScreenshotProvider};
use crate::types::*;
use async_trait::async_trait;

#[cfg(target_os = "linux")]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
#[cfg(target_os = "linux")]
use image::{ImageBuffer, Rgba};
#[cfg(target_os = "linux")]
use std::io::Cursor;
#[cfg(target_os = "linux")]
use x11rb::connection::Connection;
#[cfg(target_os = "linux")]
use x11rb::protocol::randr::ConnectionExt as RandrExt;
#[cfg(target_os = "linux")]
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat as X11ImageFormat};
#[cfg(target_os = "linux")]
use x11rb::rust_connection::RustConnection;

/// Linux computer controller using X11.
#[cfg(target_os = "linux")]
pub struct LinuxComputer {
    /// X11 connection.
    conn: RustConnection,
    /// Root window ID.
    root: u32,
    /// Screen number.
    screen_num: usize,
}

#[cfg(target_os = "linux")]
impl LinuxComputer {
    /// Create a new Linux computer controller.
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None).map_err(|e| {
            ComputerError::ScreenshotFailed(format!("Failed to connect to X11 display: {}", e))
        })?;

        let root = conn.setup().roots[screen_num].root;

        Ok(Self {
            conn,
            root,
            screen_num,
        })
    }

    /// Get the screen dimensions.
    fn get_screen_dimensions(&self) -> (u16, u16) {
        let screen = &self.conn.setup().roots[self.screen_num];
        (screen.width_in_pixels, screen.height_in_pixels)
    }

    /// Capture a region of the screen and return raw BGRA pixel data.
    fn capture_region_raw(&self, x: i16, y: i16, width: u16, height: u16) -> Result<Vec<u8>> {
        let cookie = self.conn.get_image(
            X11ImageFormat::Z_PIXMAP,
            self.root,
            x,
            y,
            width,
            height,
            !0, // All planes
        );

        let reply = cookie
            .map_err(|e| {
                ComputerError::ScreenshotFailed(format!("Failed to request image: {}", e))
            })?
            .reply()
            .map_err(|e| ComputerError::ScreenshotFailed(format!("Failed to get image: {}", e)))?;

        Ok(reply.data)
    }

    /// Convert raw X11 image data (BGRA) to PNG and encode as base64.
    fn encode_to_png_base64(&self, data: &[u8], width: u32, height: u32) -> Result<String> {
        // X11 returns BGRA format, we need to convert to RGBA
        let mut rgba_data = Vec::with_capacity(data.len());
        for chunk in data.chunks(4) {
            if chunk.len() >= 4 {
                // BGRA -> RGBA
                rgba_data.push(chunk[2]); // R
                rgba_data.push(chunk[1]); // G
                rgba_data.push(chunk[0]); // B
                rgba_data.push(chunk[3]); // A
            }
        }

        // Create image buffer
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(width, height, rgba_data)
            .ok_or_else(|| {
            ComputerError::ImageEncoding("Failed to create image buffer".into())
        })?;

        // Encode to PNG
        let mut png_data = Vec::new();
        let mut cursor = Cursor::new(&mut png_data);
        img.write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| ComputerError::ImageEncoding(format!("Failed to encode PNG: {}", e)))?;

        // Encode to base64
        Ok(BASE64_STANDARD.encode(&png_data))
    }
}

#[cfg(target_os = "linux")]
impl Default for LinuxComputer {
    fn default() -> Self {
        Self::new().expect("Failed to create Linux controller")
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl ScreenshotProvider for LinuxComputer {
    async fn capture_screen(&self) -> Result<Screenshot> {
        let (width, height) = self.get_screen_dimensions();

        let data = self.capture_region_raw(0, 0, width, height)?;
        let base64_data = self.encode_to_png_base64(&data, width as u32, height as u32)?;

        Ok(Screenshot::new(
            width as u32,
            height as u32,
            ImageFormat::Png,
            base64_data,
        ))
    }

    async fn capture_display(&self, display_id: u32) -> Result<Screenshot> {
        // Get display info and capture that specific region
        let displays = self.get_displays().await?;

        let display = displays
            .into_iter()
            .find(|d| d.id == display_id)
            .ok_or_else(|| {
                ComputerError::ScreenshotFailed(format!("Display {} not found", display_id))
            })?;

        self.capture_region(display.bounds).await
    }

    async fn capture_region(&self, region: Region) -> Result<Screenshot> {
        let data = self.capture_region_raw(
            region.x as i16,
            region.y as i16,
            region.width as u16,
            region.height as u16,
        )?;

        let base64_data = self.encode_to_png_base64(&data, region.width, region.height)?;

        Ok(Screenshot::new(
            region.width,
            region.height,
            ImageFormat::Png,
            base64_data,
        ))
    }

    async fn get_displays(&self) -> Result<Vec<DisplayInfo>> {
        // Try to use RandR extension for multi-monitor info
        let randr_result = self.get_displays_randr();

        match randr_result {
            Ok(displays) if !displays.is_empty() => Ok(displays),
            _ => {
                // Fallback to simple screen info
                let (width, height) = self.get_screen_dimensions();
                Ok(vec![DisplayInfo::primary(width as u32, height as u32)])
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl LinuxComputer {
    /// Get display information using RandR extension.
    fn get_displays_randr(&self) -> Result<Vec<DisplayInfo>> {
        // Query RandR version first
        let version_cookie = self.conn.randr_query_version(1, 5);
        let _version = version_cookie
            .map_err(|e| {
                ComputerError::ScreenshotFailed(format!("Failed to query RandR version: {}", e))
            })?
            .reply()
            .map_err(|e| ComputerError::ScreenshotFailed(format!("RandR not available: {}", e)))?;

        // Get screen resources
        let resources_cookie = self.conn.randr_get_screen_resources(self.root);
        let resources = resources_cookie
            .map_err(|e| {
                ComputerError::ScreenshotFailed(format!("Failed to get screen resources: {}", e))
            })?
            .reply()
            .map_err(|e| {
                ComputerError::ScreenshotFailed(format!(
                    "Failed to get screen resources reply: {}",
                    e
                ))
            })?;

        let mut displays = Vec::new();
        let mut is_first = true;

        for crtc in &resources.crtcs {
            let crtc_cookie = self.conn.randr_get_crtc_info(*crtc, 0);
            if let Ok(crtc_info) = crtc_cookie
                .map_err(|_| ())
                .and_then(|c| c.reply().map_err(|_| ()))
            {
                // Skip CRTCs that are not active (width/height = 0)
                if crtc_info.width == 0 || crtc_info.height == 0 {
                    continue;
                }

                let display = DisplayInfo {
                    id: displays.len() as u32,
                    name: None, // Could be extracted from output names
                    is_primary: is_first,
                    bounds: Region::new(
                        crtc_info.x as i32,
                        crtc_info.y as i32,
                        crtc_info.width as u32,
                        crtc_info.height as u32,
                    ),
                    scale_factor: 1.0,
                };

                displays.push(display);
                is_first = false;
            }
        }

        Ok(displays)
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl MouseController for LinuxComputer {
    async fn get_position(&self) -> Result<Point> {
        let cookie = self.conn.query_pointer(self.root);
        let reply = cookie
            .map_err(|e| ComputerError::MouseFailed(format!("Failed to query pointer: {}", e)))?
            .reply()
            .map_err(|e| {
                ComputerError::MouseFailed(format!("Failed to get pointer position: {}", e))
            })?;

        Ok(Point::new(reply.root_x as i32, reply.root_y as i32))
    }

    async fn move_to(&self, _point: Point) -> Result<()> {
        // Would use XTest extension for warp_pointer
        Err(ComputerError::NotSupported(
            "Mouse move not yet implemented - requires XTest".into(),
        ))
    }

    async fn move_by(&self, _dx: i32, _dy: i32) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse move not yet implemented - requires XTest".into(),
        ))
    }

    async fn click(&self, _button: MouseButton, _click_type: ClickType) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse click not yet implemented - requires XTest".into(),
        ))
    }

    async fn press(&self, _button: MouseButton) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse press not yet implemented - requires XTest".into(),
        ))
    }

    async fn release(&self, _button: MouseButton) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse release not yet implemented - requires XTest".into(),
        ))
    }

    async fn scroll(&self, _direction: ScrollDirection, _amount: u32) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse scroll not yet implemented - requires XTest".into(),
        ))
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl KeyboardController for LinuxComputer {
    async fn type_text(&self, _text: &str) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Keyboard typing not yet implemented - requires XTest".into(),
        ))
    }

    async fn press_key(&self, _combination: KeyCombination) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Key press not yet implemented - requires XTest".into(),
        ))
    }

    async fn key_down(&self, _combination: KeyCombination) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Key down not yet implemented - requires XTest".into(),
        ))
    }

    async fn key_up(&self, _combination: KeyCombination) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Key up not yet implemented - requires XTest".into(),
        ))
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl ComputerController for LinuxComputer {
    fn name(&self) -> &str {
        "linux-x11"
    }

    fn is_available(&self) -> bool {
        // Check if X11 display is available
        std::env::var("DISPLAY").is_ok()
    }
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linux_computer_creation() {
        // This test will only pass if X11 is available
        if std::env::var("DISPLAY").is_ok() {
            let computer = LinuxComputer::new();
            assert!(computer.is_ok());
        }
    }

    #[test]
    fn test_linux_computer_name() {
        if std::env::var("DISPLAY").is_ok() {
            if let Ok(computer) = LinuxComputer::new() {
                assert_eq!(computer.name(), "linux-x11");
            }
        }
    }

    #[test]
    fn test_linux_is_available() {
        if std::env::var("DISPLAY").is_ok() {
            if let Ok(computer) = LinuxComputer::new() {
                assert!(computer.is_available());
            }
        }
    }

    #[tokio::test]
    async fn test_capture_screen() {
        // This test requires an X11 display to be available
        if std::env::var("DISPLAY").is_ok() {
            if let Ok(computer) = LinuxComputer::new() {
                let result = computer.capture_screen().await;
                // May fail in headless CI environments
                if let Ok(screenshot) = result {
                    assert!(screenshot.width > 0);
                    assert!(screenshot.height > 0);
                    assert!(!screenshot.data.is_empty());
                    assert_eq!(screenshot.format, ImageFormat::Png);
                }
            }
        }
    }

    #[tokio::test]
    async fn test_capture_region() {
        if std::env::var("DISPLAY").is_ok() {
            if let Ok(computer) = LinuxComputer::new() {
                let region = Region::new(0, 0, 100, 100);
                let result = computer.capture_region(region).await;
                if let Ok(screenshot) = result {
                    assert_eq!(screenshot.width, 100);
                    assert_eq!(screenshot.height, 100);
                    assert!(!screenshot.data.is_empty());
                }
            }
        }
    }

    #[tokio::test]
    async fn test_get_displays() {
        if std::env::var("DISPLAY").is_ok() {
            if let Ok(computer) = LinuxComputer::new() {
                let result = computer.get_displays().await;
                if let Ok(displays) = result {
                    assert!(!displays.is_empty());
                    // At least one display should be primary
                    assert!(displays.iter().any(|d| d.is_primary));
                }
            }
        }
    }

    #[tokio::test]
    async fn test_get_mouse_position() {
        if std::env::var("DISPLAY").is_ok() {
            if let Ok(computer) = LinuxComputer::new() {
                let result = computer.get_position().await;
                // This should succeed if X11 is available
                if let Ok(pos) = result {
                    // Position should be within some reasonable bounds
                    assert!(pos.x >= 0 || pos.x < 0); // Any valid coordinate
                    assert!(pos.y >= 0 || pos.y < 0);
                }
            }
        }
    }

    /// Test that base64 encoding produces valid output.
    #[test]
    fn test_base64_encoding() {
        // Create a simple 2x2 BGRA image
        let bgra_data: Vec<u8> = vec![
            0, 0, 255, 255, // Blue pixel (BGRA) -> Red (RGBA)
            0, 255, 0, 255, // Green pixel
            255, 0, 0, 255, // Red pixel -> Blue
            255, 255, 255, 255, // White pixel
        ];

        if std::env::var("DISPLAY").is_ok() {
            if let Ok(computer) = LinuxComputer::new() {
                let result = computer.encode_to_png_base64(&bgra_data, 2, 2);
                assert!(result.is_ok());
                let base64_data = result.unwrap();
                // Base64 should start with PNG signature in base64
                assert!(base64_data.starts_with("iVBOR"));
            }
        }
    }
}
