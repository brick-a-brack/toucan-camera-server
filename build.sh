export PKG_CONFIG_PATH="$HOME/gphoto2/lib/pkgconfig"
export LD_LIBRARY_PATH="$HOME/gphoto2/lib:$LD_LIBRARY_PATH"
export BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/15/include"
cargo run --release --features backend-gphoto2,backend-webcam-linux,backend-remote,backend-canon
