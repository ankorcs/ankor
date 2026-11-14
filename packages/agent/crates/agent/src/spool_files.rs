//! The naming rule for the event spool's layout files in `migrations/`: `event_spool_NNNN.sql`
//! takes a file to version `NNNN`, and `event_spool_0001.sql` is the layout every file starts from.
//! The build reads the directory with [`layout_files`] when it compiles the files in.

use std::path::Path;

/// `None` for a name that breaks the rule.
#[must_use]
pub fn spool_file_version(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("event_spool_")?.strip_suffix(".sql")?;

    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    digits.parse().ok().filter(|version| *version >= 1)
}

/// The `.sql` files in `dir` with the version each reaches, in version order. Other files, such
/// as `.DS_Store`, are ignored.
///
/// # Errors
///
/// Fails when the directory cannot be read, a `.sql` file breaks the naming rule or cannot be read,
/// or there is no `event_spool_0001.sql`.
pub fn layout_files(dir: &Path) -> Result<Vec<(u32, String)>, String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut files = Vec::new();

    for entry in entries {
        let path = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?.path();
        if !path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("sql")) {
            continue;
        }

        let name = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| format!("{} is not named in UTF-8", path.display()))?;
        let version = spool_file_version(name).ok_or_else(|| format!("{name} is not named event_spool_NNNN.sql"))?;

        std::fs::read_to_string(&path).map_err(|error| format!("cannot read {name}: {error}"))?;
        files.push((version, name.to_owned()));
    }

    files.sort();

    if files.first().map(|(version, _)| *version) != Some(1) {
        return Err(format!("{} has no event_spool_0001.sql, the layout every file starts from", dir.display()));
    }

    Ok(files)
}
