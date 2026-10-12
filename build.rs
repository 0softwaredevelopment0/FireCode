fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=resources/app-icon.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("resources/app-icon.ico");
    if let Err(e) = res.compile() {
        panic!("failed to compile Windows resources: {e}");
    }
}
