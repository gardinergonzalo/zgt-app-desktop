fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "resolve_link",
                "load_link",
                "save_link",
                "clear_link",
                "open_workshop",
                "print_niimbot_b1_pro",
                "share_pdf_data",
            ]),
        ),
    )
    .expect("failed to build ZGT Desktop");
}
