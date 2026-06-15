use std::path::Path;

#[test]
fn source_tree_uses_product_concept_modules() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    for entry in [
        "canary.rs",
        "cli.rs",
        "context_engine",
        "remote_http",
        "review_sessions",
        "review_sources",
        "reviewer_kernel",
        "runner_protocol",
        "workspace",
    ] {
        assert!(
            src.join(entry).exists(),
            "expected source architecture entry `{entry}`"
        );
    }

    for forbidden in [
        "runtime",
        "runner",
        "reviewer",
        "review_session",
        "diagnostics",
        "contracts.rs",
        "repo.rs",
        "util.rs",
        "service.rs",
    ] {
        assert!(
            !src.join(forbidden).exists(),
            "forbidden top-level source bucket `{forbidden}` must not exist"
        );
    }
}
