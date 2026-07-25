use crate::config::Config;
use crate::gate::parser::{analyze_ssh_command, parse_ssh_target};

/// Check if an SSH target is a reserved/internal address (SSRF prevention).
/// Parses IPs properly including octal, hex, and decimal-encoded representations.
pub fn is_reserved_ssh_target(host: &str) -> bool {
    let host_lower = host.to_lowercase();
    let host_trimmed = host_lower.trim_matches(|c| c == '[' || c == ']');

    // Try standard IP parse first
    if let Ok(ip) = host_trimmed.parse::<std::net::IpAddr>() {
        return is_ip_reserved(ip);
    }

    // Hostname checks
    if host_trimmed == "localhost"
        || host_trimmed.ends_with(".localhost")
        || host_trimmed == "metadata.google.internal"
        || host_trimmed == "metadata.google"
    {
        return true;
    }

    // Hex-encoded IP: 0x7f000001
    if let Some(hex_part) = host_trimmed.strip_prefix("0x") {
        if let Ok(num) = u32::from_str_radix(hex_part, 16) {
            let ip = std::net::Ipv4Addr::from(num);
            return is_ipv4_reserved(ip);
        }
    }

    // Decimal-encoded IP: 2130706433
    if host_trimmed.chars().all(|c| c.is_ascii_digit())
        && !host_trimmed.is_empty()
        && host_trimmed.len() <= 10
    {
        if let Ok(num) = host_trimmed.parse::<u32>() {
            let ip = std::net::Ipv4Addr::from(num);
            return is_ipv4_reserved(ip);
        }
    }

    // Octal-encoded IP: 0177.0.0.1 (leading zeros in octets)
    if host_trimmed.contains('.') {
        let parts: Vec<&str> = host_trimmed.split('.').collect();
        if parts.len() == 4 {
            let has_octal = parts.iter().any(|p| {
                p.starts_with('0') && p.len() > 1 && p.chars().all(|c| c.is_ascii_digit())
            });
            if has_octal {
                let octets: Option<Vec<u8>> = parts
                    .iter()
                    .map(|p| {
                        if p.starts_with('0')
                            && p.len() > 1
                            && p.chars().all(|c| c.is_ascii_digit())
                        {
                            u8::from_str_radix(p, 8).ok()
                        } else {
                            p.parse::<u8>().ok()
                        }
                    })
                    .collect();
                if let Some(bytes) = octets {
                    let ip = std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
                    return is_ipv4_reserved(ip);
                }
            }
        }
    }

    false
}

/// Return whether either address family is reserved for SSH SSRF purposes.
pub(crate) fn is_ip_reserved(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_ipv4_reserved(v4),
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_ipv4_reserved(v4);
            }
            // AWS IMDSv2 alternative
            if v6.to_string() == "fd00:ec2::254" {
                return true;
            }
            false
        }
    }
}

/// Returns true if an IPv4 address must not be reached over agent SSH.
///
/// Loopback, unspecified, and link-local (which includes the 169.254.169.254
/// cloud-metadata address) are ALWAYS reserved -- they are never a legitimate
/// SSH target. RFC1918 (10/8, 172.16/12, 192.168/16), CGNAT (100.64/10), and
/// 0.0.0.0/8 are reserved BY DEFAULT but permitted when the operator sets
/// `KLEOS_NET_ALLOW_PRIVATE=1`, because agent SSH legitimately crosses a
/// private mesh (e.g. a 10.0.0.0/8 WireGuard network). Without that opt-in
/// they are treated as SSRF targets. The previous implementation omitted the
/// RFC1918/CGNAT/0.0.0.0 ranges entirely, contradicting the SSRF docstring.
pub(crate) fn is_ipv4_reserved(ip: std::net::Ipv4Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_link_local() {
        return true;
    }
    // Operator opt-in for private-mesh SSH suppresses the RFC1918 block; the
    // always-reserved ranges above still apply.
    if crate::net::allow_private_networks() {
        return false;
    }
    let octets = ip.octets();
    ip.is_private()
        // 100.64/10 CGNAT
        || (octets[0] == 100 && (octets[1] & 0xC0) == 64)
        // 0.0.0.0/8
        || octets[0] == 0
}

/// Resolve a hostname and return Some(block_reason) if any resolved IP lands
/// in a reserved/internal range. This catches DNS rebinding where the static
/// hostname check passed but the resolved address is internal (127.0.0.1,
/// 169.254.169.254 metadata, 10.0.0.0/8, etc -- the RFC1918 ranges are
/// honoured only when `KLEOS_NET_ALLOW_PRIVATE` is unset; see
/// [`is_ipv4_reserved`]). Callers should invoke this for any SSH target that
/// passed the static SSRF check.
pub async fn check_ssh_dns_rebind(host: &str, port: u16) -> Option<String> {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let addr = format!("{}:{}", host, port);
    let resolved = match tokio::net::lookup_host(addr).await {
        Ok(iter) => iter.collect::<Vec<_>>(),
        Err(e) => {
            tracing::debug!(host, error = %e, "dns lookup failed for ssh target");
            return None;
        }
    };
    for sa in resolved {
        if is_ip_reserved(sa.ip()) {
            return Some(format!(
                "SSH target {} resolves to reserved/internal address {} (DNS rebinding / SSRF prevention)",
                host,
                sa.ip()
            ));
        }
    }
    None
}

/// Validate an SSH command against static rules.
/// Returns Some(block_reason) if the command should be blocked, None if it passes.
/// Checks invocation count, transport-altering options, SSRF targets, reserved
/// IPs, and the configured reserved target list.
pub fn check_ssh_command(command: &str, config: &Config) -> Option<String> {
    let analysis = analyze_ssh_command(command);
    if analysis.invocation_count > 1 {
        return Some(
            "Multiple SSH invocations in one command are blocked because every target must be validated independently"
                .to_string(),
        );
    }
    if let Some(option) = analysis.unsafe_option {
        return Some(format!(
            "SSH option {} is blocked because it can bypass target validation",
            option
        ));
    }

    let Some(target) = analysis.target else {
        if analysis.invocation_count == 0 {
            return None;
        }
        return Some(
            "SSH invocation could not be parsed safely, so target validation cannot be guaranteed"
                .to_string(),
        );
    };
    let host = &target.host;
    let port = target.port;

    // SSRF prevention: block SSH to reserved/internal targets (hostname check)
    if is_reserved_ssh_target(host) {
        return Some(format!(
            "SSH to reserved/internal target {} blocked (SSRF prevention)",
            host
        ));
    }

    // Check config reserved_targets list
    let host_lower = host.to_lowercase();
    for reserved in &config.eidolon.gate.reserved_targets {
        if host_lower == reserved.to_lowercase() {
            return Some(format!(
                "SSH to reserved target {} blocked by configuration",
                host
            ));
        }
    }

    // Server inventory: custom-port enforcement is a warning/enrichment at the server layer
    let _ = port;

    None
}

/// Validate static SSH rules and resolve the selected hostname before the
/// command gate permits execution.
pub(crate) async fn check_ssh_command_with_dns(command: &str, config: &Config) -> Option<String> {
    if let Some(reason) = check_ssh_command(command, config) {
        return Some(reason);
    }

    let target = parse_ssh_target(command)?;
    check_ssh_dns_rebind(&target.host, target.port.unwrap_or(22)).await
}

#[cfg(test)]
/// Tests for SSH address classification and command validation.
mod tests {
    use super::*;

    // These assertions assume the default deployment posture
    // (`KLEOS_NET_ALLOW_PRIVATE` unset), which is the test environment.
    /// Metadata, loopback, unspecified, and link-local addresses stay reserved.
    #[test]
    fn metadata_and_loopback_always_reserved() {
        assert!(is_ipv4_reserved("169.254.169.254".parse().unwrap()));
        assert!(is_ipv4_reserved("127.0.0.1".parse().unwrap()));
        assert!(is_ipv4_reserved("0.0.0.0".parse().unwrap()));
        assert!(is_ipv4_reserved("169.254.10.1".parse().unwrap()));
    }

    /// Private and carrier-grade NAT ranges are reserved by default.
    #[test]
    fn rfc1918_and_cgnat_reserved_by_default() {
        // The prior implementation let these through -- the SSRF gap this fix closes.
        assert!(
            is_ipv4_reserved("10.0.0.1".parse().unwrap()),
            "10/8 must be blocked by default"
        );
        assert!(
            is_ipv4_reserved("172.16.0.1".parse().unwrap()),
            "172.16/12 must be blocked"
        );
        assert!(
            is_ipv4_reserved("192.168.1.1".parse().unwrap()),
            "192.168/16 must be blocked"
        );
        assert!(
            is_ipv4_reserved("100.64.0.1".parse().unwrap()),
            "100.64/10 CGNAT must be blocked"
        );
    }

    /// Documentation-only public ranges remain valid external targets.
    #[test]
    fn public_targets_not_reserved() {
        assert!(!is_ipv4_reserved("8.8.8.8".parse().unwrap()));
        assert!(!is_ipv4_reserved("203.0.113.7".parse().unwrap()));
    }

    /// Static target parsing rejects RFC1918 addresses in host form.
    #[test]
    fn reserved_ssh_target_blocks_rfc1918_hostform() {
        // Encoded-IP evasions resolve through is_ipv4_reserved too.
        assert!(is_reserved_ssh_target("10.0.0.5"));
        assert!(is_reserved_ssh_target("192.168.0.1"));
        assert!(!is_reserved_ssh_target("example.com"));
    }

    /// Proxy, custom configuration, and forwarding options cannot alter the
    /// validated transport path.
    #[test]
    fn transport_altering_options_are_blocked() {
        let config = Config::default();
        for command in [
            "ssh -J jump.example public.example",
            "ssh -Jjump.example public.example",
            "ssh -L 8080:127.0.0.1:80 public.example",
            "ssh -R9000:127.0.0.1:90 public.example",
            "ssh -D 1080 public.example",
            "ssh -W internal.example:22 public.example",
            "ssh -F ./alternate_config public.example",
            "ssh -o ProxyCommand='nc 127.0.0.1 22' public.example",
            "ssh -oProxyJump=jump.example public.example",
            "ssh -o LocalForward=8080:127.0.0.1:80 public.example",
            "ssh -oHostname=127.0.0.1 public.example",
        ] {
            assert!(
                check_ssh_command(command, &config).is_some(),
                "unsafe SSH option passed: {command}"
            );
        }
        assert!(
            check_ssh_command("ssh -o StrictHostKeyChecking=yes public.example", &config).is_none()
        );
    }

    /// Every local SSH token in a shell chain is detected, including pathed
    /// invocations with no surrounding whitespace.
    #[test]
    fn multiple_ssh_invocations_are_blocked() {
        let config = Config::default();
        for command in [
            "ssh public.example; ssh 127.0.0.1",
            "ssh public.example && /usr/bin/ssh 127.0.0.1",
            "ssh public.example||ssh 127.0.0.1",
            "ssh public.example | ssh 127.0.0.1",
        ] {
            let reason = check_ssh_command(command, &config).expect("chain must be blocked");
            assert!(reason.contains("Multiple SSH invocations"));
        }
    }

    /// Shell wrappers and command substitutions cannot hide an unparsed local
    /// SSH invocation from target validation.
    #[test]
    fn hidden_ssh_invocations_are_blocked() {
        let config = Config::default();
        for command in [
            "sh -c 'ssh 127.0.0.1'",
            "bash -c \"/usr/bin/ssh 127.0.0.1\"",
            "result=$(ssh 127.0.0.1)",
            "result=`ssh 127.0.0.1`",
        ] {
            assert!(
                check_ssh_command(command, &config).is_some(),
                "hidden SSH invocation passed: {command}"
            );
        }
    }

    /// The async validator resolves hostnames and rejects reserved results.
    #[tokio::test]
    async fn dns_validation_blocks_localhost_alias() {
        let reason = check_ssh_command_with_dns("ssh localhost.", &Config::default())
            .await
            .expect("localhost DNS result must be blocked");
        assert!(reason.contains("resolves to reserved/internal address"));
    }
}
