fn main() {
    println!("cargo:rerun-if-changed=assets/bibi.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/bibi.ico")
        .set("FileDescription", "BIBIIWIKI intelligent LLM wiki")
        .set("ProductName", "BIBIIWIKI")
        .set("OriginalFilename", "bibiiwiki.exe");
    resource
        .compile()
        .expect("failed to compile the BIBIIWIKI Windows icon resource");
}
