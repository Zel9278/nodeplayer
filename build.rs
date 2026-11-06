//! Builds the bundled ysfx (JSFX interpreter) as a static library.

fn main() {
    println!("cargo:rerun-if-changed=src/plugins/surface/metal_capture.mm");
    println!("cargo:rerun-if-changed=src/plugins/surface/cocoa_input.mm");
    let src = "third_party/ysfx";
    println!("cargo:rerun-if-changed={src}/sources");
    println!("cargo:rerun-if-changed={src}/include");
    println!("cargo:rerun-if-changed={src}/CMakeLists.txt");
    let static_crt = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .unwrap_or_default().split(',').any(|f| f == "crt-static");
    let dst = cmake::Config::new(src)
        .profile("Release")
        .define("YSFX_PLUGIN", "OFF")
        .define("YSFX_GFX", "ON")
        .define("YSFX_TESTS", "OFF")
        .define("YSFX_TOOLS", "OFF")
        .define("YSFX_STATIC_CRT", if static_crt { "ON" } else { "OFF" })
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
        cc::Build::new()
            .cpp(true)
            .file("src/plugins/surface/metal_capture.mm")
            .file("src/plugins/surface/cocoa_input.mm")
            .flag("-std=c++17")
            .flag("-fobjc-arc")
            .opt_level(2)
            .compile("nodeplayer_metal_capture");
        // souvlaki 0.8's build script gates MediaPlayer on the build host's cfg,
        // so Linux -> macOS builds must also declare this target dependency here.
        for fw in ["Cocoa", "Carbon", "Foundation", "Metal", "MetalKit", "QuartzCore", "MediaPlayer"] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
    }
}
