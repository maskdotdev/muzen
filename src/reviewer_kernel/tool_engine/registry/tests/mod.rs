use super::*;

#[test]
fn review_defaults_expose_familiar_model_aliases() {
    let registry = ToolRegistry::review_defaults().expect("registry");

    assert_eq!(
        registry.model_alias_for_tool(&ToolId::from(ToolName::ReadFile)),
        Some(&ToolId::parse("read").unwrap())
    );
    assert_eq!(
        registry.model_alias_for_tool(&ToolId::from(ToolName::SearchText)),
        Some(&ToolId::parse("grep").unwrap())
    );
    assert_eq!(
        registry.model_alias_for_tool(&ToolId::from(ToolName::ListFiles)),
        Some(&ToolId::parse("glob").unwrap())
    );
    assert_eq!(
        registry.tool_id_for_model_alias(&ToolId::parse("diff").unwrap()),
        Some(ToolId::from(ToolName::ReadDiff))
    );
    assert_eq!(
        registry.tool_id_for_model_alias(&ToolId::parse("imports").unwrap()),
        Some(ToolId::from(ToolName::ListImports))
    );
    assert_eq!(
        registry.tool_id_for_model_alias(&ToolId::parse("tests").unwrap()),
        Some(ToolId::from(ToolName::FindTestsForFile))
    );

    assert_eq!(
        registry.model_alias_for_tool(&ToolId::parse("missing_tool").unwrap()),
        None
    );
}
