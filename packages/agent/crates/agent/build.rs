//! Compiles the event spool's layout files in, so the binary carries every step it can run, and
//! takes `SPOOL_SCHEMA_VERSION` from the newest of them. A missing base layout, or a `.sql` file
//! that breaks the naming rule or cannot be read, fails the build.

use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

#[path = "src/spool_files.rs"]
mod spool_files;

fn main() -> Result<(), Box<dyn Error>> {
    let dir = Path::new(&std::env::var("CARGO_MANIFEST_DIR")?).join("migrations");

    println!("cargo::rerun-if-changed={}", dir.display());

    let files = spool_files::layout_files(&dir)?;
    let newest = files.last().map_or(1, |(version, _)| *version);
    let mut generated = String::new();

    writeln!(generated, "/// The layout this agent writes: the version of the newest file in `migrations/`.")?;
    writeln!(generated, "pub const SPOOL_SCHEMA_VERSION: u32 = {newest};")?;
    writeln!(generated)?;
    writeln!(generated, "/// Every file in `migrations/`, by name, in version order.")?;
    writeln!(generated, "pub const SPOOL_LAYOUT_FILES: &[(&str, &str)] = &[")?;

    for (_, name) in &files {
        writeln!(generated, "    ({name:?}, include_str!({:?})),", dir.join(name).display().to_string())?;
    }

    writeln!(generated, "];")?;
    std::fs::write(Path::new(&std::env::var("OUT_DIR")?).join("spool_layout.rs"), generated)?;
    Ok(())
}
