use std::env::var;

macro_rules! realprint {
    ($($tokens:tt)*) => {
        println!("\x1b[1;32m[SRS-BRG] =>\x1b[0m {}", format!($($tokens)*));
    }
}
macro_rules! fakeprint {
    ($($tokens:tt)*) => {
        println!("\x1b[1;36m[SRS-BRG] =>\x1b[0m {}", format!($($tokens)*));
    }
}

fn main() {
    println!("cargo:rerun-if-changed=cpp/bridge.h");
    println!("cargo:rerun-if-changed=cpp/bridge.cpp");

    realprint!("gathering bridge materials.");
    let mut build = cc::Build::new();
    build
        .warnings(false)
        .cpp(true)
        .std("c++20")
        .flag("-fno-char8_t")
        .file("cpp/bridge.cpp");

    realprint!("pulling (and applying) build flags from sys.");
    let binding = var("DEP_SURGE_BFLAGS").unwrap();
    binding.split(',').for_each(|flag| {
        fakeprint!("new flag: {}", flag);
        build.flag(flag);
    });

    realprint!("bridge is being built. please hold.");
    if let Err(error) = build.try_compile("bridge") {
        panic!("bridge burnt down while building.\n\n{error}");
    }
    println!("cargo:rustc-link-lib=static=bridge");
    realprint!("all done!");
}
