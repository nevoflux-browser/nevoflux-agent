//! Linux computer backend using X11.

use crate::error::{ComputerError, Result};
use crate::traits::{ComputerController, KeyboardController, MouseController, ScreenshotProvider};
use crate::types::*;
use async_trait::async_trait;

/// Linux computer controller using X11.
#[cfg(target_os = "linux")]
pub struct LinuxComputer {
    // X11 connection would go here
}

#[cfg(target_os = "linux")]
impl LinuxComputer {
    /// Create a new Linux computer controller.
    pub fn new() -> Result<Self> {
        Ok(Self {})
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
        // Placeholder - would use x11rb for actual capture
        Err(ComputerError::NotSupported(
            "X11 screenshot not yet implemented".into(),
        ))
    }

    async fn capture_display(&self, _display_id: u32) -> Result<Screenshot> {
        self.capture_screen().await
    }

    async fn capture_region(&self, _region: Region) -> Result<Screenshot> {
        Err(ComputerError::NotSupported(
            "Region capture not yet implemented".into(),
        ))
    }

    async fn get_displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(vec![DisplayInfo::primary(1920, 1080)])
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl MouseController for LinuxComputer {
    async fn get_position(&self) -> Result<Point> {
        Err(ComputerError::NotSupported(
            "Mouse position not yet implemented".into(),
        ))
    }

    async fn move_to(&self, _point: Point) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse move not yet implemented".into(),
        ))
    }

    async fn move_by(&self, _dx: i32, _dy: i32) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse move not yet implemented".into(),
        ))
    }

    async fn click(&self, _button: MouseButton, _click_type: ClickType) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse click not yet implemented".into(),
        ))
    }

    async fn press(&self, _button: MouseButton) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse press not yet implemented".into(),
        ))
    }

    async fn release(&self, _button: MouseButton) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse release not yet implemented".into(),
        ))
    }

    async fn scroll(&self, _direction: ScrollDirection, _amount: u32) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Mouse scroll not yet implemented".into(),
        ))
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl KeyboardController for LinuxComputer {
    async fn type_text(&self, _text: &str) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Keyboard typing not yet implemented".into(),
        ))
    }

    async fn press_key(&self, _combination: KeyCombination) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Key press not yet implemented".into(),
        ))
    }

    async fn key_down(&self, _combination: KeyCombination) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Key down not yet implemented".into(),
        ))
    }

    async fn key_up(&self, _combination: KeyCombination) -> Result<()> {
        Err(ComputerError::NotSupported(
            "Key up not yet implemented".into(),
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
        let computer = LinuxComputer::new();
        assert!(computer.is_ok());
    }

    #[test]
    fn test_linux_computer_name() {
        let computer = LinuxComputer::new().unwrap();
        assert_eq!(computer.name(), "linux-x11");
    }
}
