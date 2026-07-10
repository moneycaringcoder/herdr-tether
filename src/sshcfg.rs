use std::{collections::HashSet, fs, io, path::Path};

use anyhow::{Context, Result, bail};

/// Return literal aliases declared by `Host` directives in an OpenSSH config.
///
/// Pattern entries (including negated entries) are deliberately omitted: they
/// describe groups of hosts rather than targets a user can select explicitly.
pub fn discover_aliases(path: &Path) -> Result<Vec<String>> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("read SSH config `{}`", path.display()));
        }
    };

    let mut aliases = Vec::new();
    let mut seen = HashSet::new();

    for line in source.lines() {
        let line = line.split_once('#').map_or(line, |(before, _)| before).trim();
        if line.is_empty() {
            continue;
        }

        let (keyword, arguments) = split_directive(line);
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }

        for alias in arguments.split_ascii_whitespace() {
            if is_literal_alias(alias) && seen.insert(alias) {
                aliases.push(alias.to_owned());
            }
        }
    }

    Ok(aliases)
}

/// Validate a target that can be passed to OpenSSH as one destination argv.
///
/// Accepted forms are a literal host/alias, `user@host`, and an `ssh://` URI
/// whose authority contains an optional user and port. Options, whitespace,
/// control characters, paths, queries, fragments, and shell punctuation are
/// rejected rather than relying on a shell or OpenSSH to reinterpret them.
pub fn validate_ssh_target(target: &str) -> Result<()> {
    if target.is_empty() {
        bail!("SSH target must not be empty");
    }
    if target.contains('\0') {
        bail!("SSH target must not contain NUL");
    }
    if target.chars().any(char::is_whitespace) {
        bail!("SSH target must not contain whitespace");
    }

    if target
        .get(..6)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ssh://"))
    {
        validate_ssh_uri(&target[6..])
    } else {
        validate_destination(target)
    }
}

fn split_directive(line: &str) -> (&str, &str) {
    let boundary = line
        .char_indices()
        .find_map(|(index, character)| (character.is_ascii_whitespace() || character == '=').then_some(index));

    match boundary {
        Some(index) => {
            let rest = line[index..]
                .trim_start_matches(|character: char| character.is_ascii_whitespace())
                .strip_prefix('=')
                .unwrap_or(&line[index..])
                .trim_start_matches(|character: char| character.is_ascii_whitespace());
            (&line[..index], rest)
        }
        None => (line, ""),
    }
}

fn is_literal_alias(alias: &str) -> bool {
    !alias.is_empty()
        && !alias.starts_with('!')
        && !alias.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']'))
        && is_host(alias)
}

fn validate_destination(destination: &str) -> Result<()> {
    if destination.starts_with('-') {
        bail!("SSH target must not begin with an option marker");
    }

    let mut parts = destination.split('@');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some() {
        bail!("SSH target contains more than one user separator");
    }

    let (user, host) = match second {
        Some(host) => (Some(first), host),
        None => (None, first),
    };
    if let Some(user) = user {
        validate_user(user)?;
    }
    if !is_host(host) {
        bail!("SSH target contains an invalid host or alias");
    }
    Ok(())
}

fn validate_ssh_uri(authority: &str) -> Result<()> {
    if authority.is_empty() {
        bail!("SSH URI must contain a host");
    }
    if authority
        .bytes()
        .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b';' | b'\\'))
    {
        bail!("SSH URI must contain only an authority");
    }

    let (user, host_and_port) = match authority.split_once('@') {
        Some((user, remainder)) if !remainder.contains('@') => (Some(user), remainder),
        Some(_) => bail!("SSH URI contains more than one user separator"),
        None => (None, authority),
    };
    if let Some(user) = user {
        validate_user(user)?;
    }

    if let Some(rest) = host_and_port.strip_prefix('[') {
        let Some((address, suffix)) = rest.split_once(']') else {
            bail!("SSH URI has an unterminated IPv6 address");
        };
        if address.is_empty()
            || !address.contains(':')
            || !address
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || matches!(byte, b':' | b'.'))
        {
            bail!("SSH URI contains an invalid IPv6 address");
        }
        validate_port_suffix(suffix)?;
        return Ok(());
    }

    let (host, port) = match host_and_port.split_once(':') {
        Some((host, port)) if !port.contains(':') => (host, Some(port)),
        Some(_) => bail!("IPv6 addresses in SSH URIs must be enclosed in brackets"),
        None => (host_and_port, None),
    };
    if !is_host(host) {
        bail!("SSH URI contains an invalid host or alias");
    }
    if let Some(port) = port {
        validate_port(port)?;
    }
    Ok(())
}

fn validate_user(user: &str) -> Result<()> {
    if user.is_empty()
        || !user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("SSH target contains an invalid user");
    }
    Ok(())
}

fn is_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn validate_port_suffix(suffix: &str) -> Result<()> {
    if suffix.is_empty() {
        return Ok(());
    }
    let Some(port) = suffix.strip_prefix(':') else {
        bail!("unexpected data after SSH URI host");
    };
    validate_port(port)
}

fn validate_port(port: &str) -> Result<()> {
    let number = port
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("SSH URI contains an invalid port"))?;
    if number == 0 {
        bail!("SSH URI port must be between 1 and 65535");
    }
    Ok(())
}
