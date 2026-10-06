fn main() {
    println!("cargo:rerun-if-changed=cpp/overlay.cpp");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    cc::Build::new()
        .cpp(true)
        .file("cpp/overlay.cpp")
        .flag("-std=c++17")
        .cpp_link_stdlib("c++_static")
        .compile("overlay");
}
