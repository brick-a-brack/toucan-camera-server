#[cfg(feature = "backend-canon")]
pub mod canon;

#[cfg(all(feature = "backend-gphoto2", any(target_os = "linux", target_os = "macos")))]
pub mod gphoto2;

#[cfg(all(feature = "backend-webcam-macos", target_os = "macos"))]
pub mod webcam_macos;

#[cfg(all(feature = "backend-webcam-windows", target_os = "windows"))]
pub mod webcam_windows;

#[cfg(all(feature = "backend-camera2-android", target_os = "android"))]
pub mod camera2_android;

#[cfg(feature = "backend-remote")]
pub mod remote;
