//! The Lens protos, generated at build time from Chromium's own `.proto` files.
//!
//! The sources are vendored under `third_party/lens_server_proto`, pinned to
//! the Chromium commit in `CHROMIUM_COMMIT` beside them, and compiled by
//! `build.rs` - which also explains why the editions files take a detour
//! through a descriptor set on the way to prost.
//!
//! Re-pin with `cargo run --bin update-protos -- --ref <branch|tag|commit>`.
//!
//! Everything here is in Chromium's `lens` package, so it arrives as one flat
//! module. This replaced a hand-transcribed subset: about two hundred lines of
//! prost structs with field numbers typed in by hand, where a wrong tag fails
//! silently rather than loudly, and every field Chromium added had to be
//! noticed and copied by a person.

// Generated code: lint it on prost's terms, not ours.
#![allow(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    rustdoc::bare_urls,
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_html_tags
)]

include!(concat!(env!("OUT_DIR"), "/lens.rs"));
