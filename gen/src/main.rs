//! A program which generates a linux-headers installation and runs bindgen
//! over each public header, for each supported architecture, for a selection
//! of Linux kernel versions.

use bindgen::{builder, EnumVariation};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

#[allow(unused_doc_comments)]
const LINUX_VERSIONS: [&str; 8] = [
    /// Base supported revisions for various architectures.
    /// <https://doc.rust-lang.org/nightly/rustc/platform-support.html>
    "v2.6.32",
    "v3.2",
    "v3.10",
    "v4.2",
    "v4.4",
    "v4.20",
    /// This is the oldest kernel version available on Github Actions.
    /// <https://github.com/actions/virtual-environments#available-environments>
    "v5.4",
    /// Linux 5.6 has `openat2` so pick something newer than that.
    "v5.11",
];

/// Base supported revisions for various architectures.
/// <https://doc.rust-lang.org/nightly/rustc/platform-support.html>
const DEFAULT_LINUX_VERSIONS: [(&str, &str); 9] = [
    ("x86", "v2.6.32"),
    ("x86_64", "v2.6.32"),
    ("aarch64", "v4.2"),
    ("mips", "v4.4"),
    ("mips64", "v4.4"),
    ("arm", "v3.2"),
    ("powerpc", "v2.6.32"),
    ("powerpc64", "v3.10"), // powerpc64 has 2.6.32, but powerpc64le has 3.10; go with the later for now.
    ("riscv64", "v4.20"),
];

/// Some commonly used features.
const DEFAULT_FEATURES: &str = "\"general\", \"errno\"";

fn main() {
    let mut args = env::args();
    let _exe = args.next().unwrap();
    let cmd = args.next();

    // This is the main invocation path.
    assert!(cmd.is_none());
    assert!(args.next().is_none());

    git_init();

    let out = tempdir::TempDir::new("linux-raw-sys").unwrap();
    let out_dir = out.path();
    let linux_headers = out_dir.join("linux-headers");
    let linux_include = linux_headers.join("include");

    // Clean up any modules from previous builds.
    for entry in fs::read_dir("../src").unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.path().to_str().unwrap().ends_with("."));
        if entry.file_type().unwrap().is_dir() {
            fs::remove_dir_all(entry.path()).ok();
        }
    }

    // Edit ../src/lib.rs
    let mut src_lib_rs_in = File::open("../src/lib.rs").unwrap();
    let mut src_lib_rs_contents = String::new();
    src_lib_rs_in
        .read_to_string(&mut src_lib_rs_contents)
        .unwrap();
    let edit_at = src_lib_rs_contents
        .find("// The rest of this file is auto-generated!\n")
        .unwrap();
    src_lib_rs_contents = src_lib_rs_contents[..edit_at].to_owned();

    let mut src_lib_rs = File::create("../src/lib.rs").unwrap();
    src_lib_rs
        .write_all(src_lib_rs_contents.as_bytes())
        .unwrap();
    src_lib_rs
        .write_all("// The rest of this file is auto-generated!\n".as_bytes())
        .unwrap();

    // Edit ../Cargo.toml
    let mut cargo_toml_in = File::open("../Cargo.toml").unwrap();
    let mut cargo_toml_contents = String::new();
    cargo_toml_in
        .read_to_string(&mut cargo_toml_contents)
        .unwrap();
    let edit_at = cargo_toml_contents
        .find("# The rest of this file is auto-generated!\n")
        .unwrap();
    cargo_toml_contents = cargo_toml_contents[..edit_at].to_owned();

    // Generate Cargo.toml
    let mut cargo_toml = File::create("../Cargo.toml").unwrap();
    cargo_toml
        .write_all(cargo_toml_contents.as_bytes())
        .unwrap();
    cargo_toml
        .write_all("# The rest of this file is auto-generated!\n".as_bytes())
        .unwrap();
    writeln!(cargo_toml, "[features]").unwrap();

    let mut features: HashSet<String> = HashSet::new();

    for linux_version in &LINUX_VERSIONS {
        let linux_version_mod = linux_version.replace('.', "_");

        // Collect all unique feature names across all architectures.
        if features.insert(linux_version_mod.clone()) {
            writeln!(cargo_toml, "{} = []", linux_version_mod).unwrap();
        }

        // Define the module. If this isn't the default version, make it
        // conditional.
        let default_arch_versions = DEFAULT_LINUX_VERSIONS
            .iter()
            .filter(|default| &default.1 == linux_version)
            .map(|default| default.0)
            .collect::<Vec<_>>();
        if !default_arch_versions.is_empty() {
            let mut cfg_versions = vec![];
            for arch in default_arch_versions {
                cfg_versions.push(format!("target_arch = \"{}\"", arch));
            }
            writeln!(src_lib_rs, "{}", gen_cfg_any(&cfg_versions)).unwrap();
            writeln!(src_lib_rs, "pub mod {};", linux_version_mod).unwrap();

            // If this is the default version for an architecture, make the
            // contents available in the top-level namespace.
            writeln!(src_lib_rs, "{}", gen_cfg_any(&cfg_versions)).unwrap();
            writeln!(src_lib_rs, "pub use {}::*;", linux_version_mod).unwrap();
        } else {
            let cfg_version = format!("#[cfg(feature = \"{}\")]", linux_version_mod);
            writeln!(src_lib_rs, "{}", cfg_version).unwrap();
            writeln!(src_lib_rs, "pub mod {};", linux_version_mod).unwrap();
        }

        let src_vers = format!("../src/{}", linux_version_mod);
        fs::create_dir_all(&src_vers).unwrap();
        let mut src_vers_mod_rs = File::create(&format!("{}/mod.rs", src_vers)).unwrap();

        // Checkout a specific version of Linux.
        git_checkout(linux_version);

        let mut linux_archs = fs::read_dir(&format!("linux/arch"))
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        // Sort archs list as filesystem iteration order is non-deterministic
        linux_archs.sort_by_key(|entry| entry.file_name());
        for linux_arch_entry in linux_archs {
            if !linux_arch_entry.file_type().unwrap().is_dir() {
                continue;
            }
            let linux_arch = linux_arch_entry.file_name().to_str().unwrap().to_owned();

            let rust_arches = rust_arches(&linux_arch);
            if rust_arches.is_empty() {
                continue;
            }

            fs::create_dir_all(&linux_headers).unwrap();

            let mut headers_made = false;
            for rust_arch in rust_arches {
                // Only build the default versions on their associated
                // architectures.
                if !DEFAULT_LINUX_VERSIONS
                    .iter()
                    .any(|default| rust_arch == &default.0 && linux_version == &default.1)
                    && DEFAULT_LINUX_VERSIONS
                        .iter()
                        .any(|default| linux_version == &default.1)
                {
                    continue;
                }

                if !headers_made {
                    make_headers_install(&linux_arch, &linux_headers);
                    headers_made = true;
                }

                eprintln!(
                    "Generating all bindings for Linux {} architecture {}",
                    linux_version, rust_arch
                );

                let src_arch = format!("{}/{}", src_vers, rust_arch);
                fs::create_dir_all(&src_arch).unwrap();
                let mut src_arch_mod_rs = File::create(&format!("{}/mod.rs", src_arch)).unwrap();

                let cfg_arch = format!("#[cfg(target_arch = \"{}\")]", rust_arch);
                writeln!(src_vers_mod_rs, "{}", cfg_arch).unwrap();
                writeln!(src_vers_mod_rs, "mod {};", rust_arch).unwrap();
                writeln!(src_vers_mod_rs, "{}", cfg_arch).unwrap();
                writeln!(src_vers_mod_rs, "pub use {}::*;", rust_arch).unwrap();

                let mut modules = fs::read_dir("modules")
                    .unwrap()
                    .map(|entry| entry.unwrap())
                    .collect::<Vec<_>>();
                // Sort module list as filesystem iteration order is non-deterministic
                modules.sort_by_key(|entry| entry.file_name());
                for mod_entry in modules {
                    let header_name = mod_entry.path();
                    let mod_name = header_name.file_stem().unwrap().to_str().unwrap();
                    let mod_rs = format!("{}/{}.rs", src_arch, mod_name);

                    run_bindgen(
                        linux_include.to_str().unwrap(),
                        header_name.to_str().unwrap(),
                        &mod_rs,
                        mod_name,
                        rust_arch,
                        linux_version,
                    );

                    writeln!(src_arch_mod_rs, "/// {}", header_name.to_str().unwrap()).unwrap();
                    writeln!(src_arch_mod_rs, "#[cfg(feature = \"{}\")]", mod_name).unwrap();
                    writeln!(src_arch_mod_rs, "pub mod r#{};", mod_name).unwrap();
                    // Collect all unique feature names across all architectures.
                    if features.insert(mod_name.to_owned()) {
                        writeln!(cargo_toml, "{} = []", mod_name).unwrap();
                    }
                }
            }

            fs::remove_dir_all(&linux_headers).unwrap();
        }
    }

    writeln!(cargo_toml, "default = [\"std\", {}]", DEFAULT_FEATURES).unwrap();
    writeln!(cargo_toml, "std = []").unwrap();
    writeln!(cargo_toml, "no_std = []").unwrap();
    writeln!(
        cargo_toml,
        "rustc-dep-of-std = [\"core\", \"compiler_builtins\", \"no_std\"]"
    )
    .unwrap();

    // Reset the `linux` directory back to the original branch.
    git_checkout(LINUX_VERSIONS[0]);

    eprintln!("All bindings generated!");
}

fn git_init() {
    // Clone the linux kernel source repo if necessary. Ignore exit code as it will be non-zero in
    // case it was already cloned.
    // Use a treeless partial clone to save disk space and clone time.
    // See https://github.blog/2020-12-21-get-up-to-speed-with-partial-clone-and-shallow-clone/ for
    // more info on partial clones.
    // Note: this is not using the official repo
    // git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git but the github fork as the
    // server of the official repo doesn't recognize filtering.
    if !Path::new("linux/.git").exists() {
        assert!(Command::new("git")
            .arg("clone")
            .arg("https://github.com/torvalds/linux.git")
            .arg("--filter=tree:0")
            .arg("--no-checkout")
            .status()
            .unwrap()
            .success());
    }

    // Setup sparse checkout. This greatly reduces the amount of objects necessary to checkout the
    // tree.
    assert!(Command::new("git")
        .arg("sparse-checkout")
        .arg("init")
        .current_dir("linux")
        .status()
        .unwrap()
        .success());

    fs::write(
        "linux/.git/info/sparse-checkout",
        "/*
!/*/
/include/
/arch/
/scripts/
/tools/",
    )
    .unwrap();
}

fn git_checkout(rev: &str) {
    // Delete any generated files from previous versions.
    assert!(Command::new("git")
        .arg("clean")
        .arg("-f")
        .arg("-d")
        .current_dir("linux")
        .status()
        .unwrap()
        .success());

    // Check out the given revision.
    assert!(Command::new("git")
        .arg("checkout")
        .arg(rev)
        .arg("-f")
        .current_dir("linux")
        .status()
        .unwrap()
        .success());

    // Delete any untracked generated files from previous versions.
    assert!(Command::new("git")
        .arg("clean")
        .arg("-f")
        .arg("-d")
        .current_dir("linux")
        .status()
        .unwrap()
        .success());
}

fn make_headers_install(linux_arch: &str, linux_headers: &Path) {
    assert!(Command::new("make")
        .arg(format!("headers_install"))
        .arg(format!("ARCH={}", linux_arch))
        .arg(format!(
            "INSTALL_HDR_PATH={}",
            fs::canonicalize(&linux_headers).unwrap().to_str().unwrap()
        ))
        .current_dir("linux")
        .status()
        .unwrap()
        .success());
}

fn rust_arches(linux_arch: &str) -> &[&str] {
    match linux_arch {
        "arm" => &["arm"],
        "arm64" => &["aarch64"],
        "avr32" => &["avr"],
        // hexagon gets build errors; disable it for now
        "hexagon" => &[],
        "mips" => &["mips", "mips64"],
        "powerpc" => &["powerpc", "powerpc64"],
        "riscv" => &["riscv32", "riscv64"],
        "s390" => &["s390x"],
        "sparc" => &["sparc", "sparc64"],
        "x86" => &["x86", "x86_64"],
        "alpha" | "cris" | "h8300" | "m68k" | "microblaze" | "mn10300" | "score" | "blackfin"
        | "frv" | "ia64" | "m32r" | "m68knommu" | "parisc" | "sh" | "um" | "xtensa"
        | "unicore32" | "c6x" | "nios2" | "openrisc" | "csky" | "arc" | "nds32" | "metag"
        | "tile" => &[],
        _ => panic!("unrecognized arch: {}", linux_arch),
    }
}

fn run_bindgen(
    linux_include: &str,
    header_name: &str,
    mod_rs: &str,
    mod_name: &str,
    rust_arch: &str,
    linux_version: &str,
) {
    let clang_arch = compute_clang_arch(rust_arch);

    eprintln!(
        "Generating bindings for {} on Linux {} architecture {}",
        mod_name, linux_version, rust_arch
    );

    let builder = builder()
        // The generated bindings are quite large, so use a few simple options
        // to keep the file sizes down.
        .rustfmt_configuration_file(Some(Path::new("bindgen-rustfmt.toml").to_owned()))
        .layout_tests(false)
        .generate_comments(false)
        .default_enum_style(EnumVariation::Rust {
            non_exhaustive: true,
        })
        .array_pointers_in_arguments(true)
        .derive_debug(true)
        .clang_arg(&format!("--target={}-unknown-linux", clang_arch))
        .clang_arg("-DBITS_PER_LONG=(__SIZEOF_LONG__*__CHAR_BIT__)")
        .clang_arg("-nostdinc")
        .clang_arg("-I")
        .clang_arg(linux_include)
        .clang_arg("-I")
        .clang_arg("include")
        .blocklist_item("NULL");

    let bindings = builder
        .use_core()
        .ctypes_prefix("crate::ctypes")
        .header(header_name)
        .generate()
        .expect(&format!("generate bindings for {}", mod_name));
    bindings
        .write_to_file(mod_rs)
        .expect(&format!("write_to_file for {}", mod_name));
}

fn compute_clang_arch(rust_arch: &str) -> &str {
    if rust_arch == "x86" {
        "i686"
    } else {
        rust_arch
    }
}

fn gen_cfg_any(cfgs: &[String]) -> String {
    match &cfgs[..] {
        [] => String::new(),
        [cfg] => format!("#[cfg({})]", cfg),
        cfgs => format!("#[cfg(any({}))]", cfgs.join(", ")),
    }
}
