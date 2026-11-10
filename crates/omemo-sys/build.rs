//! Builds the vendored libomemo-c (+ protobuf-c runtime) and generates bindings.

use std::path::{Path, PathBuf};

fn c_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "c"))
        .collect();
    out.sort();
    out
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../third_party");
    let src = root.join("libomemo-c/src");
    let ed = src.join("curve25519/ed25519");
    let pbc = root.join("protobuf-c");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed=wrapper.h");

    let include_dirs = vec![
        src.clone(),
        src.join("curve25519"),
        ed.clone(),
        ed.join("nacl_includes"),
        ed.join("additions"),
        ed.join("additions/generalized"),
        ed.join("sha512"),
        ed.join("tests"),
        pbc.clone(),
    ];

    let system = std::env::var_os("CARGO_FEATURE_SYSTEM").is_some();
    let mut clang_includes = include_dirs.clone();
    if system {
        let lib = pkg_config::Config::new()
            .atleast_version("0.5")
            .probe("libomemo-c")
            .expect("libomemo-c not found via pkg-config (install it or disable the `system` feature)");
        clang_includes = lib.include_paths.clone();
        clang_includes.extend(lib.include_paths.iter().map(|p| p.join("omemo")));
    } else {
        let mut build = cc::Build::new();
        build.warnings(false).flag_if_supported("-w");
        for d in &include_dirs {
            build.include(d);
        }
        let mut files = c_files(&src);
        files.push(src.join("curve25519/curve25519-donna.c"));
        files.extend(c_files(&ed));
        files.extend(c_files(&ed.join("additions")));
        files.extend(c_files(&ed.join("additions/generalized")));
        files.extend(c_files(&ed.join("nacl_sha512")));
        files.push(ed.join("tests/internal_fast_tests.c"));
        files.push(pbc.join("protobuf-c/protobuf-c.c"));
        build.files(files).compile("omemo-c");
    }

    let mut b = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_function(
            "signal_.*|session_.*|curve_.*|ec_.*|ratchet_.*|pre_key_.*|ciphertext_message_.*|signal_protocol_.*",
        )
        .allowlist_type("signal_.*|session_.*|ec_.*|ratchet_.*|pre_key_.*|ciphertext_message|signal_message")
        .allowlist_var("SG_.*|CIPHERTEXT_.*|PRE_KEY_.*|SIGNAL_.*|DJB_.*|OMEMO_.*")
        .layout_tests(false)
        .derive_default(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for d in &clang_includes {
        b = b.clang_arg(format!("-I{}", d.display()));
    }
    let bindings = b.generate().expect("generating libomemo-c bindings");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings.write_to_file(out).expect("writing bindings");
}
