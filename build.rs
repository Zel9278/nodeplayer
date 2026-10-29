//! Builds the bundled ysfx (JSFX interpreter) as a static library.

fn main() {
    let src = "third_party/ysfx";
    println!("cargo:rerun-if-changed={src}/sources");
    println!("cargo:rerun-if-changed={src}/include");
    println!("cargo:rerun-if-changed={src}/CMakeLists.txt");
    let dst = cmake::Config::new(src)
        .profile("Release")
        .define("YSFX_PLUGIN", "OFF")
        .define("YSFX_GFX", "ON")
        .define("YSFX_TESTS", "OFF")
        .define("YSFX_TOOLS", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .build_target("ysfx")
        .build();
    println!("cargo:rustc-link-search=native={}/build", dst.display());
    println!("cargo:rustc-link-lib=static=ysfx");
    for lib in ["stdc++", "fontconfig", "freetype", "dl", "pthread", "m"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
