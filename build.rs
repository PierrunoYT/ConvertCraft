//! Embeds `assets/logos/logo.png` as the Windows executable icon so Explorer,
//! the taskbar, and shortcuts show the logo (the runtime window icon alone
//! does not affect the .exe file).

fn main() {
    println!("cargo:rerun-if-changed=assets/logos/logo.png");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_icon();
    }
}

#[cfg(windows)]
fn embed_icon() {
    use image::codecs::ico::{IcoEncoder, IcoFrame};
    use image::imageops::FilterType;

    let logo = image::open("assets/logos/logo.png").expect("read assets/logos/logo.png");
    let frames: Vec<IcoFrame> = [256u32, 64, 48, 32, 16]
        .iter()
        .map(|&size| {
            let rgba = logo
                .resize_exact(size, size, FilterType::Lanczos3)
                .into_rgba8();
            IcoFrame::as_png(rgba.as_raw(), size, size, image::ExtendedColorType::Rgba8)
                .expect("encode icon frame")
        })
        .collect();

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out_dir.join("convertcraft.ico");
    let file = std::fs::File::create(&ico_path).expect("create icon file");
    IcoEncoder::new(file)
        .encode_images(&frames)
        .expect("write icon file");

    winresource::WindowsResource::new()
        .set_icon(ico_path.to_str().expect("UTF-8 OUT_DIR"))
        .compile()
        .expect("embed Windows icon resource");
}
