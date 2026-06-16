use super::*;

#[test]
fn review_defaults_expose_familiar_model_aliases() {
    let registry = ToolRegistry::review_defaults().expect("registry");
    let table = registry.alias_table().expect("alias table");

    assert_eq!(
        table.alias_for(&ToolId::from(ToolName::ReadFile)),
        Some(&ToolId::parse("read").unwrap())
    );
    assert_eq!(
        table.alias_for(&ToolId::from(ToolName::SearchText)),
        Some(&ToolId::parse("grep").unwrap())
    );
    assert_eq!(
        table.alias_for(&ToolId::from(ToolName::ListFiles)),
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

    assert!(registry.alias_table().is_ok());
}
