// On macOS the EDSDK registers its IOKit USB-detection sources on the main CF
// run loop (CFRunLoopGetMain). We must keep the main thread free to pump it,
// so tokio runs on a background thread instead.
#[cfg(target_os = "macos")]
fn main() {
    std::thread::spawn(|| {
        tokio::runtime::Runtime::new()
            .expect("failed to build tokio runtime")
            .block_on(toucan_camera_server_lib::run_server());
    });

    // Pump the main CF run loop forever. The EDSDK's IOKit USB notifications
    // fire here, making cameras visible to EdsGetCameraList.
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRunLoopRun();
    }
    unsafe { CFRunLoopRun() };
}

#[cfg(not(target_os = "macos"))]
#[tokio::main]
async fn main() {
    toucan_camera_server_lib::run_server().await;
}
