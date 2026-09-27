/// Maintenance policy lives in top-level instructions, never ordered input.
pub fn validate_maintenance_input(input: &[crate::ModelRequestItem<'_>]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !input.iter().any(|item| matches!(
            item,
            crate::ModelRequestItem::DeveloperInstruction(_)
                | crate::ModelRequestItem::RequestInstruction(_)
        )),
        "maintenance input must not contain ordered developer instructions"
    );
    Ok(())
}
