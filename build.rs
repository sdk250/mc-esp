fn main() {
    println!("cargo:rerun-if-changed=cpp/overlay.cpp");

    cc::Build::new()
        .cpp(true)
        .file("cpp/overlay.cpp")
        .flag("-std=c++17")
        .cpp_link_stdlib("c++_static")
        .compile("overlay");
}
