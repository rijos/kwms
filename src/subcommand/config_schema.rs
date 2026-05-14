//! `kwms config schema` — emit JSON Schema for the merged config shape.

pub fn run() -> anyhow::Result<()> {
    let schema = crate::config::schema::emit_json_schema()?;
    println!("{schema}");
    Ok(())
}
