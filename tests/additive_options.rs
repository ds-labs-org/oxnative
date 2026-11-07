//! `DbOptions` must be identical with and without the `vault` feature (#14):
//! the feature is additive, so a crate that unifies it on must keep compiling.

use oxnative::DbOptions;

/// Exhaustive literal, as written by the Oxigraph fork: no `..Default`. It
/// stops compiling if the feature adds a field.
#[test]
fn exhaustive_db_options_literal_builds_with_any_feature_set() {
    let options = DbOptions {
        max_open_files: Some(16),
        fd_reserve: None,
    };
    assert_eq!(options.max_open_files, Some(16), "field is kept");
    assert_eq!(options.fd_reserve, None, "field is kept");
}

#[test]
fn db_options_is_copy_with_any_feature_set() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<DbOptions>();
}
