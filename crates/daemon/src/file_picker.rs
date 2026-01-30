//! Native file picker dialog support.

use nevoflux_protocol::{
    FileInfo, PickFilesError, PickFilesRequest, PickFilesResponse, PickerMode,
};
use std::path::Path;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Lock to prevent concurrent file picker dialogs
static FILE_PICKER_LOCK: Mutex<()> = Mutex::const_new(());

/// Create FileInfo from a path by reading metadata
pub fn file_info_from_path(path: &Path) -> std::io::Result<FileInfo> {
    let metadata = std::fs::metadata(path)?;
    Ok(FileInfo {
        path: path.to_string_lossy().to_string(),
        is_directory: metadata.is_dir(),
        size: if metadata.is_file() {
            Some(metadata.len())
        } else {
            None
        },
        modified: metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
    })
}

/// Check if a graphical display is available (Linux only)
#[cfg(target_os = "linux")]
fn has_display() -> bool {
    std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok()
}

#[cfg(not(target_os = "linux"))]
fn has_display() -> bool {
    true
}

/// Pick files or directories using the native file dialog.
///
/// This function is async but will block the main thread while the dialog is open.
/// Only one dialog can be open at a time.
pub async fn pick_files(req: PickFilesRequest) -> Result<PickFilesResponse, PickFilesError> {
    // Check for display on Linux
    if !has_display() {
        return Err(PickFilesError::NoDisplay);
    }

    // Prevent concurrent dialogs
    let _guard = match FILE_PICKER_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(_) => return Err(PickFilesError::AlreadyPicking),
    };

    debug!(
        "Opening file picker: mode={:?}, multiple={}, title={:?}",
        req.mode, req.multiple, req.title
    );

    // Build the dialog
    let mut dialog = rfd::AsyncFileDialog::new();

    if let Some(title) = &req.title {
        dialog = dialog.set_title(title);
    }

    if let Some(path) = &req.default_path {
        let p = Path::new(path);
        if p.exists() && p.is_dir() {
            dialog = dialog.set_directory(p);
        } else {
            warn!(
                "Default path does not exist or is not a directory: {}",
                path
            );
        }
    }

    // Pick based on mode
    let handles = match req.mode {
        PickerMode::Files | PickerMode::Both => {
            if req.multiple {
                dialog.pick_files().await
            } else {
                dialog.pick_file().await.map(|f| vec![f])
            }
        }
        PickerMode::Directories => {
            if req.multiple {
                dialog.pick_folders().await
            } else {
                dialog.pick_folder().await.map(|f| vec![f])
            }
        }
    };

    // Convert handles to FileInfo
    match handles {
        Some(files) => {
            let file_infos: Vec<FileInfo> = files
                .into_iter()
                .filter_map(|f| {
                    let path = f.path();
                    match file_info_from_path(path) {
                        Ok(info) => Some(info),
                        Err(e) => {
                            warn!("Failed to read metadata for {:?}: {}", path, e);
                            None
                        }
                    }
                })
                .collect();

            debug!("Selected {} files", file_infos.len());

            Ok(PickFilesResponse {
                files: file_infos,
                cancelled: false,
            })
        }
        None => {
            debug!("File picker cancelled");
            Ok(PickFilesResponse {
                files: vec![],
                cancelled: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    #[test]
    fn test_file_info_from_path_file() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("test.txt");
        File::create(&file_path).unwrap();

        let info = file_info_from_path(&file_path).unwrap();
        assert!(!info.is_directory);
        assert!(info.size.is_some());
        assert!(info.modified.is_some());
        assert!(info.path.ends_with("test.txt"));
    }

    #[test]
    fn test_file_info_from_path_directory() {
        let temp = TempDir::new().unwrap();

        let info = file_info_from_path(temp.path()).unwrap();
        assert!(info.is_directory);
        assert!(info.size.is_none());
        assert!(info.modified.is_some());
    }

    #[test]
    fn test_file_info_from_path_not_found() {
        let result = file_info_from_path(Path::new("/nonexistent/path/file.txt"));
        assert!(result.is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_has_display_checks_env() {
        // This test just verifies the function doesn't panic
        let _ = has_display();
    }
}
