//! Throwaway: which of the Nikon auto-ISO properties actually answers?
use gphoto2::widget::RadioWidget;
use gphoto2::Context;

fn read(camera: &gphoto2::Camera, key: &str) -> String {
    match camera.config_key::<RadioWidget>(key).wait() {
        Ok(w) => format!("{:?}{}", w.choice(), if w.readonly() { " [ro]" } else { "" }),
        Err(e) => format!("<{e}>"),
    }
}

fn write(camera: &gphoto2::Camera, key: &str, choice: &str) -> String {
    match camera.config_key::<RadioWidget>(key).wait() {
        Err(e) => format!("<{e}>"),
        Ok(w) => {
            if let Err(e) = w.set_choice(choice) {
                return format!("set_choice failed: {e}");
            }
            match camera.set_config(&w).wait() {
                Ok(()) => "OK".to_string(),
                Err(e) => format!("ERROR {e}"),
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::env::set_var("LC_ALL", "C");
    let camera = Context::new()?.autodetect_camera().wait()?;
    let dump = |c: &gphoto2::Camera, when: &str| {
        eprintln!(
            "{when:<22} isoauto={:<12} autoiso={:<12} iso={}",
            read(c, "isoauto"),
            read(c, "autoiso"),
            read(c, "iso")
        );
    };
    dump(&camera, "initial");
    eprintln!("write autoiso=On  -> {}", write(&camera, "autoiso", "On"));
    dump(&camera, "after autoiso=On");
    eprintln!("write autoiso=Off -> {}", write(&camera, "autoiso", "Off"));
    dump(&camera, "after autoiso=Off");
    eprintln!("write isoauto=Off -> {}", write(&camera, "isoauto", "Off"));
    dump(&camera, "after isoauto=Off");
    Ok(())
}
