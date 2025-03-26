use vergen_gix::{Emitter, GixBuilder};

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Only emit git enabled variables if they're valid (in a git tree)
    let gix = GixBuilder::default().dirty(true).sha(true).build()?;
    Emitter::default()
        .add_instructions(&gix)?
        .fail_on_error()
        .emit()?;
    Ok(())
}
