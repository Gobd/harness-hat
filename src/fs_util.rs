use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    set_private_file_permissions(path)?;
    Ok(())
}

pub(crate) fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    let _ = path;
    Ok(())
}

pub(crate) fn write_env_file_entry<W: Write>(
    writer: &mut W,
    key: &str,
    value: impl AsRef<str>,
) -> Result<()> {
    let value = value.as_ref();
    validate_env_file_entry(key, value)?;
    writeln!(writer, "{key}={value}")?;
    Ok(())
}

pub(crate) fn validate_env_file_entry(key: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        is_valid_env_name(key),
        "invalid environment variable name for Docker env file: {key}"
    );
    anyhow::ensure!(
        !value.contains('\n') && !value.contains('\r'),
        "environment variable value for {key} must not contain newlines"
    );
    Ok(())
}

pub(crate) fn is_valid_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::write_env_file_entry;

    #[test]
    fn env_file_entry_rejects_newline_values() {
        let mut out = Vec::new();
        let err = write_env_file_entry(&mut out, "TOKEN", "abc\nINJECTED=1")
            .expect_err("newlines must be rejected");

        assert!(err.to_string().contains("must not contain newlines"));
    }

    #[test]
    fn env_file_entry_rejects_invalid_names() {
        let mut out = Vec::new();
        let err = write_env_file_entry(&mut out, "BAD-NAME", "value")
            .expect_err("invalid names must be rejected");

        assert!(
            err.to_string()
                .contains("invalid environment variable name")
        );
    }

    #[test]
    fn env_file_entry_writes_valid_line() {
        let mut out = Vec::new();

        write_env_file_entry(&mut out, "GOOD_NAME", "value").expect("write env entry");

        assert_eq!(String::from_utf8(out).unwrap(), "GOOD_NAME=value\n");
    }
}
