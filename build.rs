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
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let build_dir = dst.join("build");
    let link_dir = if env == "msvc" {
        let release_dir = build_dir.join("Release");
        if release_dir.join("ysfx.lib").is_file() {
            release_dir
        } else if build_dir.join("ysfx.lib").is_file() {
            build_dir
        } else {
            panic!(
                "ysfx.lib was not found in {} or {}",
                release_dir.display(),
                build_dir.display()
            );
        }
    } else {
        build_dir
    };
    println!("cargo:rustc-link-search=native={}", link_dir.display());
    println!("cargo:rustc-link-lib=static=ysfx");
    // What ysfx's graphics (LICE, and SWELL outside Windows) need from the system.
    let libs: &[&str] = match (os.as_str(), env.as_str()) {
        ("windows", "msvc") => &["gdi32", "user32", "shell32", "comdlg32"],
        ("windows", _) => &["gdi32", "user32", "shell32", "comdlg32"],
        ("macos", _) => &["c++"],
        _ => &["stdc++", "fontconfig", "freetype", "dl", "pthread", "m"],
    };
    for lib in libs {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    // MinGW's C++ runtime goes in statically, so the .exe needs no DLLs beside it.
    if os == "windows" && env == "gnu" {
        println!("cargo:rustc-link-lib=static=stdc++");
        println!("cargo:rustc-link-lib=static=winpthread");
        println!("cargo:rustc-link-arg=-static-libgcc");
    }
    if os == "macos" {
        for fw in ["Cocoa", "Carbon", "Foundation", "Metal"] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
    }
}
