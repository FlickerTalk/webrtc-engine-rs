//! Builds the vendored libopus with the `cc` crate.
//!
//! Every crate that ships libopus (`opus`, `opusic-sys`, `audiopus_sys`,
//! `opus-static-sys`...) builds it with cmake, which is not available on the
//! build machines, so we compile the plain C sources ourselves. The source
//! lists come from libopus' own `*_sources.mk` files so an upgrade of the
//! vendored tree needs no change here.
//!
//! Cross builds: for iOS set `IPHONEOS_DEPLOYMENT_TARGET` (Xcode does), otherwise clang
//! stamps the objects with the SDK version as minimum OS. For Android point
//! `CC_aarch64_linux_android` and `AR_aarch64_linux_android` at the NDK's clang and llvm-ar.

use std::fs;
use std::path::{Path, PathBuf};

const OPUS_DIR: &str = "vendor/opus";

/// Float build, no SIMD, no DNN (deep PLC, DRED, OSCE), no custom modes.
const SOURCE_LISTS: &[(&str, &str)] = &[
    ("celt_sources.mk", "CELT_SOURCES"),
    ("silk_sources.mk", "SILK_SOURCES"),
    ("silk_sources.mk", "SILK_SOURCES_FLOAT"),
    ("opus_sources.mk", "OPUS_SOURCES"),
    ("opus_sources.mk", "OPUS_SOURCES_FLOAT"),
];

fn main() {
    let root = Path::new(OPUS_DIR);
    println!("cargo:rerun-if-changed={OPUS_DIR}");

    let mut build = cc::Build::new();
    for dir in ["include", "celt", "silk", "silk/float"] {
        build.include(root.join(dir));
    }
    build
        .define("OPUS_BUILD", None)
        .define("VAR_ARRAYS", None)
        .define("HAVE_LRINT", None)
        .define("HAVE_LRINTF", None)
        .warnings(false);

    for (file, variable) in SOURCE_LISTS {
        let mk_path = root.join(file);
        let text = fs::read_to_string(&mk_path)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", mk_path.display()));
        let sources = make_list(&text, variable);
        assert!(!sources.is_empty(), "{variable} not found in {file}");
        build.files(sources.iter().map(|source| root.join(source)));
    }

    build.compile("opus");
}

/// Returns the entries of `NAME = a \ b \ c` in a makefile fragment.
fn make_list(text: &str, name: &str) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if !inside {
            match line.split_once('=') {
                Some((key, rest)) if key.trim() == name => {
                    inside = true;
                    push_entries(rest, &mut entries);
                    if !rest.trim_end().ends_with('\\') {
                        break;
                    }
                }
                _ => {}
            }
            continue;
        }
        push_entries(line, &mut entries);
        if !line.ends_with('\\') {
            break;
        }
    }
    entries
}

fn push_entries(fragment: &str, entries: &mut Vec<PathBuf>) {
    entries.extend(
        fragment
            .split_whitespace()
            .filter(|word| *word != "\\")
            .map(PathBuf::from),
    );
}

// Cargo does not run build script tests. After a build, run them with:
// rustc --edition 2024 --test build.rs -o target/build-rs-tests \
//   --extern cc=$(ls target/debug/deps/libcc-*.rlib) -L target/debug/deps && target/build-rs-tests
#[cfg(test)]
mod tests {
    use super::*;

    const MK: &str = "A = \\\nx/a.c \\\nx/b.c\n\nA_MORE = \\\nx/c.c\n";

    #[test]
    fn make_list_reads_a_continued_variable() {
        assert_eq!(
            make_list(MK, "A"),
            vec![PathBuf::from("x/a.c"), PathBuf::from("x/b.c")]
        );
    }

    #[test]
    fn make_list_does_not_match_a_prefix() {
        assert_eq!(make_list(MK, "A_MORE"), vec![PathBuf::from("x/c.c")]);
        assert!(make_list(MK, "MISSING").is_empty());
    }
}
