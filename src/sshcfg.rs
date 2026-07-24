use std::{
    collections::HashSet,
    env, fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const MAX_INCLUDE_DEPTH: usize = 4;
const MAX_INCLUDE_FILES: usize = 64;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Return literal aliases declared by `Host` directives in an OpenSSH config.
///
/// Relative and absolute `Include` paths are followed only when their canonical
/// target remains beneath the primary config's canonical directory. Literal
/// paths plus `*`/`?` filename globs are supported with finite depth, file, and
/// byte budgets. Pattern Host entries remain excluded because they describe
/// groups rather than explicit user-selectable targets.
pub fn discover_aliases(path: &Path) -> Result<Vec<String>> {
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("resolve SSH config `{}`", path.display()));
        }
    };
    let root = canonical
        .parent()
        .context("SSH config must have a parent directory")?
        .to_path_buf();
    let mut discovery = AliasDiscovery {
        root,
        visited: HashSet::new(),
        aliases: Vec::new(),
        seen_aliases: HashSet::new(),
        total_bytes: 0,
    };
    discovery.visit(&canonical, 0)?;
    Ok(discovery.aliases)
}

struct AliasDiscovery {
    root: PathBuf,
    visited: HashSet<PathBuf>,
    aliases: Vec<String>,
    seen_aliases: HashSet<String>,
    total_bytes: u64,
}

impl AliasDiscovery {
    fn visit(&mut self, path: &Path, depth: usize) -> Result<()> {
        if depth > MAX_INCLUDE_DEPTH {
            return Ok(());
        }
        let canonical = match fs::canonicalize(path) {
            Ok(path) if path.starts_with(&self.root) => path,
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("resolve included SSH config `{}`", path.display()));
            }
        };
        if self.visited.contains(&canonical) {
            return Ok(());
        }
        if self.visited.len() >= MAX_INCLUDE_FILES {
            bail!("SSH Include traversal exceeded {MAX_INCLUDE_FILES} files");
        }
        let metadata = fs::metadata(&canonical)
            .with_context(|| format!("inspect SSH config `{}`", canonical.display()))?;
        if !metadata.is_file() {
            return Ok(());
        }
        self.total_bytes = self
            .total_bytes
            .checked_add(metadata.len())
            .context("SSH Include byte budget overflow")?;
        if self.total_bytes > MAX_CONFIG_BYTES {
            bail!("SSH Include traversal exceeded {MAX_CONFIG_BYTES} bytes");
        }
        self.visited.insert(canonical.clone());
        let source = fs::read_to_string(&canonical)
            .with_context(|| format!("read SSH config `{}`", canonical.display()))?;

        for line in source.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (keyword, arguments) = split_directive(line);
            if keyword.eq_ignore_ascii_case("host") {
                for alias in arguments.split_ascii_whitespace() {
                    if is_literal_alias(alias) && self.seen_aliases.insert(alias.to_owned()) {
                        self.aliases.push(alias.to_owned());
                    }
                }
            } else if keyword.eq_ignore_ascii_case("include") && depth < MAX_INCLUDE_DEPTH {
                for pattern in arguments.split_ascii_whitespace() {
                    let includes = expand_include(&self.root, pattern)?;
                    for include in includes {
                        self.visit(&include, depth + 1)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn expand_include(base: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    if pattern.is_empty()
        || pattern.bytes().any(|byte| {
            byte.is_ascii_control() || matches!(byte, b'[' | b']' | b'\\' | b'"' | b'\'')
        })
    {
        return Ok(Vec::new());
    }
    let path = if let Some(relative) = pattern.strip_prefix("~/") {
        let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
            return Ok(Vec::new());
        };
        home.join(relative)
    } else {
        let path = PathBuf::from(pattern);
        if path.is_absolute() {
            path
        } else {
            base.join(path)
        }
    };
    let Some(file_pattern) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(Vec::new());
    };
    if !file_pattern.contains(['*', '?']) {
        return Ok(vec![path]);
    }
    let Some(parent) = path.parent() else {
        return Ok(Vec::new());
    };
    if parent.as_os_str().to_string_lossy().contains(['*', '?']) {
        return Ok(Vec::new());
    }
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read SSH Include directory `{}`", parent.display()));
        }
    };
    let mut matches = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| wildcard_match(file_pattern.as_bytes(), name.as_bytes()))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    matches.sort();
    Ok(matches)
}

fn wildcard_match(pattern: &[u8], value: &[u8]) -> bool {
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star, mut retry_value) = (None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            retry_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            retry_value += 1;
            value_index = retry_value;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

#[derive(Clone, Debug)]
pub(crate) struct OpenSshTarget {
    pub(crate) destination: String,
    pub(crate) port: Option<u16>,
}
pub(crate) fn openssh_connection_args(interactive: bool) -> Vec<String> {
    let mut arguments = vec![
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        "ConnectTimeout=10".to_owned(),
        "-o".to_owned(),
        "ControlMaster=auto".to_owned(),
        "-o".to_owned(),
        "ControlPersist=60".to_owned(),
        "-o".to_owned(),
        "ControlPath=~/.ssh/herdr-tether-%C".to_owned(),
    ];
    if interactive {
        arguments.push("-t".to_owned());
    }
    arguments.extend([
        "-o".to_owned(),
        "ServerAliveInterval=15".to_owned(),
        "-o".to_owned(),
        "ServerAliveCountMax=3".to_owned(),
    ]);
    arguments
}

pub(crate) fn openssh_target(target: &str) -> Result<OpenSshTarget> {
    validate_ssh_target(target)?;
    let Some(authority) = target.strip_prefix("ssh://") else {
        return Ok(OpenSshTarget {
            destination: target.to_owned(),
            port: None,
        });
    };
    let (user, host_and_port) = authority
        .split_once('@')
        .map_or((None, authority), |(user, remainder)| {
            (Some(user), remainder)
        });
    let (host, port) = if let Some(rest) = host_and_port.strip_prefix('[') {
        let (address, suffix) = rest
            .split_once(']')
            .expect("validated SSH URI has a closing IPv6 bracket");
        (address, suffix.strip_prefix(':'))
    } else {
        host_and_port
            .split_once(':')
            .map_or((host_and_port, None), |(host, port)| (host, Some(port)))
    };
    let destination = user.map_or_else(|| host.to_owned(), |user| format!("{user}@{host}"));
    Ok(OpenSshTarget {
        destination,
        port: port.map(|port| {
            port.parse()
                .expect("validated SSH URI port is a nonzero u16")
        }),
    })
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

    if let Some(authority) = target.strip_prefix("ssh://") {
        validate_ssh_uri(authority)
    } else {
        validate_destination(target)
    }
}

fn split_directive(line: &str) -> (&str, &str) {
    let boundary = line.char_indices().find_map(|(index, character)| {
        (character.is_ascii_whitespace() || character == '=').then_some(index)
    });

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
        && !alias
            .bytes()
            .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']'))
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
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'#'))
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
