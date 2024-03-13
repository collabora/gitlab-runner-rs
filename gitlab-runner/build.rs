use vergen::EmitBuilder;

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Only emit git enabled variables if they're valid (in a git tree)
    let _ = EmitBuilder::builder()
        .git_dirty(true)
        .git_sha(true)
        .fail_on_error()
        .emit();
    Ok(())
}
