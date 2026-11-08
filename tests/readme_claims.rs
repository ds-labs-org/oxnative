//! Doc-claims checklist (#12): statements of the README and of the rustdoc
//! that were wrong must stay fixed, and the names they mention must exist.

#[cfg(test)]
mod tests {
    const README: &str = include_str!("../README.md");
    const LIB: &str = include_str!("../src/lib.rs");

    const INDEX_CFS: [&str; 9] = [
        "spog", "posg", "ospg", "gspo", "gpos", "gosp", "dspo", "dpos", "dosp",
    ];

    fn section<'a>(text: &'a str, heading: &str) -> &'a str {
        let start = text.find(heading).unwrap_or(0);
        let rest = &text[start + heading.len()..];
        let end = rest
            .find("\n## ")
            .or_else(|| rest.find("\n### "))
            .unwrap_or(rest.len());
        &rest[..end]
    }

    #[test]
    fn index_column_family_count_matches_the_list() {
        assert!(!README.contains("six index column families"));
        assert!(README.contains("nine index column families"));
        for cf in INDEX_CFS {
            assert!(README.contains(&format!("`{cf}`")), "{cf} must be listed");
        }
    }

    #[test]
    fn fork_guidance_states_inlining_and_unkeyed_hash() {
        let fork = section(README, "### Using it from the Oxigraph fork");
        for needle in ["15 bytes", "inline", "SipHash", "keyed hash", "no inlining"] {
            assert!(
                fork.contains(needle),
                "fork guidance must mention {needle:?}"
            );
        }
        assert!(
            !fork.contains("nothing is lost\nby leaving it plaintext"),
            "the claim that id2str's plaintext key loses nothing is false"
        );
    }

    #[test]
    fn threat_model_does_not_claim_rollback_or_transplant_detection() {
        assert!(
            !README
                .contains("cannot\nmodify, swap or roll back individual values without detection")
        );
        assert!(
            !README
                .contains("cannot modify, swap or roll back individual values without detection")
        );
        let threat = section(README, "### Threat model");
        assert!(threat.contains("replay"), "value replay under its own key");
        assert!(
            threat.contains("shar") && threat.contains("between databases"),
            "sealed bytes copied between databases sharing a key"
        );
    }

    #[test]
    fn format_section_matches_the_marker_and_entry_points() {
        let format = section(README, "### On-disk format");
        let dedented = format.lines().map(str::trim).collect::<Vec<_>>().join("\n");
        assert!(dedented.contains("oxnative 2\nengine fjall 3\nvault 1"));
        assert!(
            format.contains("oxnative 1"),
            "legacy first line still opens"
        );
        for api in ["open_read_write_with_vault", "open_read_only_with_vault"] {
            assert!(README.contains(api));
            assert!(LIB.contains(&format!("pub fn {api}")));
        }
        assert!(LIB.contains("\"oxnative 2\""));
    }

    #[test]
    fn backup_rustdoc_does_not_claim_plain_open_for_vaulted_copies() {
        let doc = section(LIB, "/// Writes a consistent copy of the database");
        let doc = doc.split("pub fn backup").next().unwrap_or(doc);
        assert!(doc.contains("open_read_write_with_vault"), "{doc}");
    }
}
