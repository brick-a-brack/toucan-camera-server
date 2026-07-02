#[cfg(all(feature = "backend-canon", any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub mod canon;

#[cfg(all(feature = "backend-nikon-zs2", any(target_os = "macos", target_os = "windows")))]
pub mod nikon_zs2;

#[cfg(all(feature = "backend-webcam-linux", target_os = "linux"))]
pub mod webcam_linux;

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
