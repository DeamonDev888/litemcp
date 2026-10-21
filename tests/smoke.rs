//! Tests de fumée : le crate se compile et expose son contrat minimal.

#[test]
fn crate_version_is_exposed() {
    assert!(!litemcp::VERSION.is_empty());
}
