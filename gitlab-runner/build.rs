use vergen_gitcl::{Emitter, GitclBuilder};

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Only emit git enabled variables if they're valid (in a git tree)
    let _ = Emitter::default()
        .add_instructions(&GitclBuilder::default().dirty(true).sha(true).build()?)?
        .fail_on_error()
        .emit();
    Ok(())
}
