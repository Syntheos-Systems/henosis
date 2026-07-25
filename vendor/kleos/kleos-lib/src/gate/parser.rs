/// Parsed representation of an SSH command target.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// Internal SSH command metadata used by the security validator.
#[derive(Debug, Clone)]
pub(crate) struct SshCommandAnalysis {
    /// Number of SSH tokens found across the command.
    pub invocation_count: usize,
    /// Target parsed from the first SSH invocation.
    pub target: Option<SshTarget>,
    /// First transport-altering option found before the target.
    pub unsafe_option: Option<String>,
}

/// True when a command token invokes ssh: matched by basename so absolute
/// invocations (`/usr/bin/ssh`, `/opt/homebrew/bin/ssh`) are recognized.
/// Exact-token matching let any pathed invocation bypass SSRF detection
/// entirely, which is the security-relevant consumer of this parser.
fn is_ssh_token(token: &str) -> bool {
    token == "ssh" || token.rsplit('/').next() == Some("ssh")
}

/// Count SSH executable lexemes without trusting quote grouping. This
/// conservative pass exposes invocations hidden inside shell `-c` strings,
/// command substitutions, or backticks so the validator can fail closed.
fn ssh_lexeme_count(command: &str) -> usize {
    command
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/'))
        })
        .filter(|token| is_ssh_token(token))
        .count()
}

/// Split a shell command into quote-aware tokens while treating command
/// separators as boundaries. Quoted remote commands stay within one token,
/// while local `;`, `&&`, `||`, and pipe chains expose each SSH invocation.
fn shell_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in command.chars() {
        if escaped {
            token.push(character);
            escaped = false;
            continue;
        }

        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    token.push(character);
                }
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    escaped = true;
                } else {
                    token.push(character);
                }
            }
            Some(_) => unreachable!("only shell quote characters are stored"),
            None => match character {
                '\'' | '"' => quote = Some(character),
                '\\' => escaped = true,
                ';' | '|' | '&' | '\n' => {
                    if !token.is_empty() {
                        tokens.push(std::mem::take(&mut token));
                    }
                }
                character if character.is_whitespace() => {
                    if !token.is_empty() {
                        tokens.push(std::mem::take(&mut token));
                    }
                }
                _ => token.push(character),
            },
        }
    }

    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

/// Return the canonical name of an SSH option that can change the connection
/// target, proxy route, forwarding path, or execute a local helper.
fn unsafe_ssh_option(token: &str, following: Option<&str>) -> Option<String> {
    const UNSAFE_SHORT_OPTIONS: [&str; 6] = ["-F", "-J", "-L", "-R", "-D", "-W"];
    const UNSAFE_CONFIG_KEYS: [&str; 9] = [
        "dynamicforward",
        "hostname",
        "include",
        "localcommand",
        "localforward",
        "permitlocalcommand",
        "proxycommand",
        "proxyjump",
        "remoteforward",
    ];

    if let Some(option) = UNSAFE_SHORT_OPTIONS
        .iter()
        .find(|option| token == **option || token.starts_with(**option) && token.len() > 2)
    {
        return Some((*option).to_string());
    }

    let option_value = if token == "-o" {
        following?
    } else {
        token.strip_prefix("-o")?
    };
    let key = option_value
        .trim_start_matches('=')
        .split(|character: char| character == '=' || character.is_whitespace())
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    UNSAFE_CONFIG_KEYS
        .contains(&key.as_str())
        .then(|| format!("-o {}", key))
}

/// Analyze all SSH tokens and the first invocation's target and options.
pub(crate) fn analyze_ssh_command(command: &str) -> SshCommandAnalysis {
    let tokens = shell_tokens(command);
    let invocation_count = ssh_lexeme_count(command);
    let ssh_positions: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| is_ssh_token(token).then_some(index))
        .collect();
    let Some(&ssh_pos) = ssh_positions.first() else {
        return SshCommandAnalysis {
            invocation_count,
            target: None,
            unsafe_option: None,
        };
    };

    let mut host_raw = None;
    let mut port = None;
    let mut unsafe_option = None;
    let mut index = ssh_pos + 1;

    while index < tokens.len() {
        let token = tokens[index].as_str();
        let following = tokens.get(index + 1).map(String::as_str);

        if let Some(option) = unsafe_ssh_option(token, following) {
            unsafe_option = Some(option);
            break;
        }
        if token == "--" {
            index += 1;
            host_raw = tokens.get(index).map(String::as_str);
            break;
        }
        if matches!(token, "-p" | "-P") {
            index += 1;
            port = tokens
                .get(index)
                .and_then(|value| value.parse::<u16>().ok());
        } else if let Some(value) = token.strip_prefix("-p").filter(|value| !value.is_empty()) {
            port = value.parse::<u16>().ok();
        } else if token.starts_with('-') {
            if matches!(
                token,
                "-B" | "-b"
                    | "-c"
                    | "-E"
                    | "-e"
                    | "-I"
                    | "-i"
                    | "-l"
                    | "-m"
                    | "-O"
                    | "-o"
                    | "-S"
                    | "-w"
            ) {
                index += 1;
            }
        } else if !token.contains('=') {
            host_raw = Some(token);
            break;
        }
        index += 1;
    }

    let target = host_raw.map(|raw| {
        let (user, host) = if let Some(position) = raw.rfind('@') {
            (
                Some(raw[..position].to_string()),
                raw[position + 1..].to_string(),
            )
        } else {
            (None, raw.to_string())
        };
        SshTarget { user, host, port }
    });

    SshCommandAnalysis {
        invocation_count,
        target,
        unsafe_option,
    }
}

/// Parse an SSH command string to extract the target host, user, and port.
/// Used for SSRF detection and server map lookups. Leading environment
/// wrappers (`env`, `VAR=value` assignments) before the ssh token are
/// skipped by the token scan itself.
pub fn parse_ssh_target(command: &str) -> Option<SshTarget> {
    analyze_ssh_command(command).target
}

/// Generate enrichment context for a systemctl command.
/// Returns a human-readable description of the action and service name if parseable.
/// Used to inject context into gate responses.
pub fn check_systemctl_command(command: &str) -> Option<String> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let systemctl_pos = tokens.iter().position(|&t| t == "systemctl")?;

    let action = tokens.get(systemctl_pos + 1).copied().unwrap_or("");
    let service = tokens
        .iter()
        .skip(systemctl_pos + 2)
        .find(|&&t| !t.starts_with('-'));

    let service = service.copied()?;

    Some(format!(
        "systemctl {} {} - verify restart order and service dependencies before proceeding",
        action, service
    ))
}

/// Detect `{{secret:...}}` or `{{secret-raw:...}}` placeholders in a string.
pub fn has_secret_placeholders(input: &str) -> bool {
    input.contains("{{secret:") || input.contains("{{secret-raw:")
}
