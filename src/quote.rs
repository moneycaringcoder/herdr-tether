use anyhow::{Result, bail};

/// Quotes one value as a single POSIX shell word.
///
/// The returned word is always single-quoted. Embedded apostrophes are emitted
/// by ending the quoted region, escaping the apostrophe, and reopening it.
/// NUL cannot be represented in a shell command and is therefore rejected.
pub fn posix_quote(value: &str) -> Result<String> {
    if value.as_bytes().contains(&0) {
        bail!("cannot quote a value containing NUL");
    }

    let apostrophes = value.bytes().filter(|byte| *byte == b'\'').count();
    let mut quoted = String::with_capacity(value.len() + 2 + apostrophes * 3);
    quoted.push('\'');
    for (index, part) in value.split('\'').enumerate() {
        if index != 0 {
            quoted.push_str("'\\''");
        }
        quoted.push_str(part);
    }
    quoted.push('\'');
    Ok(quoted)
}
