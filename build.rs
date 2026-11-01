//! Compile the vendored Lens protos with a pinned protoc.
//!
//! The protos live in `third_party/lens_server_proto`, vendored from one
//! Chromium commit recorded in `CHROMIUM_COMMIT` beside them. Re-pin them with
//! `cargo run --bin update-protos -- --ref <branch|tag|commit>`.
//!
//! protoc comes from `protoc-bin-vendored` rather than the host, because most
//! of these files declare `edition = "2023"` or `"2024"` and a protoc below 33
//! cannot parse them - and older protoc *skips* such a file rather than
//! failing, which is how the equivalent checked-in modules in chrome-lens-py
//! went stale once without anyone noticing.
//!
//! # Why this goes through a descriptor set
//!
//! prost-build cannot read editions files at all: it maps `syntax` onto a
//! two-value enum and panics with "unknown syntax: editions" on anything else
//! (prost-build 0.14.4, `code_generator/syntax.rs`; tokio-rs/prost#1031 is open
//! with no fix). 28 of the 45 Chromium files are editions, so that is not a
//! corner we can avoid.
//!
//! protoc itself understands them. So protoc produces a FileDescriptorSet, and
//! we rewrite each editions file's `syntax` to the pre-editions syntax with the
//! same field-presence semantics before handing it to prost:
//!
//!   * a file with `option features.field_presence = IMPLICIT;` behaves like
//!     proto3 - a singular scalar has no presence and is skipped when empty -
//!     so it becomes `proto3`.
//!   * every other file keeps edition 2023's default, EXPLICIT, which is what
//!     `optional` means in proto2, so it becomes `proto2`.
//!
//! The other features in these files line up with that mapping rather than
//! fighting it: `enum_type = CLOSED` and `utf8_validation = NONE` are both the
//! proto2 defaults, and every file that asks for them is in the EXPLICIT group.
//!
//! One field does not fit, and the mismatch is deliberate and harmless:
//! `ImageCrop.crop_id` in `lens_overlay_image_crop.proto` overrides presence to
//! IMPLICIT for itself inside an otherwise EXPLICIT file, so it comes out as
//! `Option<String>` instead of `String`. That changes the Rust ergonomics, not
//! the wire format, and nothing here writes that field.
//!
//! A per-field override *is* in the descriptor set - protoc writes it to
//! `FieldOptions.features`, which is field 21 (not 50, which is
//! `FileOptions.features`; `MessageOptions` uses 12 and `EnumOptions` 7). prost
//! simply ignores it and decides presence from the file's `syntax` alone, so
//! the override is lost whatever we put in the descriptor. The source text is
//! the cheapest place to notice one, and we vendor it - so
//! `warn_about_per_field_overrides` scans for them and names any that is not
//! already known and accepted.

use std::path::{Path, PathBuf};
use std::process::Command;

use prost::Message;

const PROTO_DIR: &str = "third_party/lens_server_proto";

/// The file-level option that makes an editions file behave like proto3.
const IMPLICIT_OPTION: &str = "option features.field_presence = IMPLICIT;";

/// True if `source` sets implicit field presence for the whole file.
///
/// Column matters: the same option indented is scoped to a message or a field,
/// and must not be read as applying to the file.
fn declares_file_level_implicit(source: &str) -> bool {
    source
        .lines()
        .any(|line| line.trim_end() == IMPLICIT_OPTION)
}

/// Fields whose own presence override we knowingly do not honour.
///
/// * `ImageCrop.crop_id` asks for IMPLICIT inside an otherwise EXPLICIT file,
///   so it comes out `Option<String>` where strictly it should be `String`.
///   Ergonomics, not wire format, and nothing here writes the field.
/// * `TextLayout.Word.text_separator` is the other way round: EXPLICIT inside
///   an IMPLICIT file, so it comes out `String` and an absent separator is
///   indistinguishable from an empty one. Chromium's comment says that
///   distinction is deliberate - but nothing acts on it, here or in
///   chrome-lens-py, which concatenates `plain_text + text_separator` and so
///   reads an absent separator as empty too. Honouring it would mean an
///   `Option<String>` that every caller unwraps to `""`.
const ACCEPTED_PER_FIELD_OVERRIDES: &[&str] = &[
    "lens_overlay_image_crop.proto:crop_id",
    "lens_overlay_text.proto:text_separator",
];

/// Name any per-field presence override that is not already accounted for.
///
/// These are invisible downstream - see the module docs - so the source scan is
/// the only warning anyone gets. A warning rather than a hard failure: an
/// override on a field this crate never touches should not stop a build.
fn warn_about_per_field_overrides(name: &str, source: &str) {
    for line in source.lines() {
        if !line.contains("[features.field_presence") {
            continue;
        }
        // `string crop_id = 1 [features.field_presence = IMPLICIT];`
        let field = line
            .split('=')
            .next()
            .unwrap_or(line)
            .split_whitespace()
            .last()
            .unwrap_or("?");
        let key = format!("{name}:{field}");
        if ACCEPTED_PER_FIELD_OVERRIDES.contains(&key.as_str()) {
            continue;
        }
        println!(
            "cargo::warning={key} overrides field presence for itself, which a \
             descriptor set cannot carry; it is generated with its file's \
             presence instead. Check whether this crate writes that field."
        );
    }
}

fn main() {
    let dir = Path::new(PROTO_DIR);

    // A changed, added or removed proto has to trigger a rebuild; so does the
    // pin file, which is what a re-pin touches even when no content moved.
    println!("cargo::rerun-if-changed={PROTO_DIR}");
    println!("cargo::rerun-if-changed=build.rs");

    let mut protos: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {PROTO_DIR}: {e}"))
        .map(|entry| entry.expect("reading a directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "proto"))
        .collect();
    protos.sort();

    assert!(
        !protos.is_empty(),
        "no .proto files in {PROTO_DIR}; run `cargo run --bin update-protos`"
    );

    let commit = std::fs::read_to_string(dir.join("CHROMIUM_COMMIT"))
        .unwrap_or_else(|e| panic!("cannot read {PROTO_DIR}/CHROMIUM_COMMIT: {e}"));
    println!(
        "cargo::warning=compiling {} Lens protos from chromium@{}",
        protos.len(),
        commit.trim().get(..12).unwrap_or("?")
    );

    for path in &protos {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a proto file name");
        let source =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {name}: {e}"));
        warn_about_per_field_overrides(name, &source);
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let fds_path = out_dir.join("lens_protos.fds");

    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    let output = Command::new(&protoc)
        .arg(format!("--proto_path={}", dir.display()))
        .arg(format!("--descriptor_set_out={}", fds_path.display()))
        .arg("--include_imports")
        .args(&protos)
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", protoc.display()));

    if !output.status.success() {
        panic!(
            "protoc failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let bytes =
        std::fs::read(&fds_path).unwrap_or_else(|e| panic!("reading {}: {e}", fds_path.display()));
    let mut fds = prost_types::FileDescriptorSet::decode(&*bytes)
        .unwrap_or_else(|e| panic!("decoding the descriptor set: {e}"));

    let mut rewritten = 0usize;
    for file in &mut fds.file {
        if file.syntax.as_deref() != Some("editions") {
            continue;
        }
        let name = file.name.clone().unwrap_or_default();
        let source = std::fs::read_to_string(dir.join(&name))
            .unwrap_or_else(|e| panic!("re-reading {name} to classify its presence: {e}"));

        let syntax = if declares_file_level_implicit(&source) {
            "proto3"
        } else {
            "proto2"
        };
        // Only `syntax` needs clearing: prost-types' descriptor.proto predates
        // editions and has no `edition` field to carry, and prost reads nothing
        // else about the edition.
        file.syntax = Some(syntax.to_owned());
        rewritten += 1;
    }
    println!("cargo::warning=rewrote the syntax of {rewritten} editions file(s) for prost");

    // Nothing may reach prost still claiming to be editions: it would panic
    // with a message that says nothing about this build script.
    let leftover: Vec<&str> = fds
        .file
        .iter()
        .filter(|f| f.syntax.as_deref() == Some("editions"))
        .filter_map(|f| f.name.as_deref())
        .collect();
    assert!(
        leftover.is_empty(),
        "these files are still editions after the rewrite: {leftover:?}"
    );

    prost_build::Config::new()
        .compile_fds(fds)
        .expect("generating Rust from the Lens descriptor set");
}
