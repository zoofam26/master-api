//! Local, RFC-compliant IPv4/IPv6 parsing, address-scope classification and
//! caller-address resolution.
//!
//! Nothing in this module touches the network: every answer is a pure function
//! of the input text. That makes it safe to run before every upstream call (to
//! avoid spending the free tier on addresses that can never have geolocation
//! data) and safe to unit-test exhaustively.
//!
//! Ported 1:1 from `ip-geolocation-api/src/geolocation/ip/ip-address.ts` and
//! `client-address.ts`.

use master_common::error::ApiError;
use serde_json::json;

pub type IpVersion = u8;

pub const SCOPE_PUBLIC: &str = "public";
pub const SCOPE_PRIVATE: &str = "private";
pub const SCOPE_LOOPBACK: &str = "loopback";
pub const SCOPE_LINK_LOCAL: &str = "link-local";
pub const SCOPE_MULTICAST: &str = "multicast";
pub const SCOPE_RESERVED: &str = "reserved";
pub const SCOPE_UNSPECIFIED: &str = "unspecified";
pub const SCOPE_BROADCAST: &str = "broadcast";
pub const SCOPE_CARRIER_GRADE_NAT: &str = "carrier-grade-nat";

/// A strictly parsed and classified address.
#[derive(Debug, Clone)]
pub struct ParsedIpAddress {
    /// `4` or `6`.
    pub version: IpVersion,
    /// Canonical text form: dotted quad, or RFC 5952 compressed lowercase IPv6.
    pub address: String,
    /// Lowercase big-endian hex of the raw bytes; a stable identity.
    pub hex: String,
    /// Classification derived purely from the address bits.
    pub scope: &'static str,
    /// True only for globally routable unicast addresses.
    pub is_public: bool,
}

/* ---------------------------------------------------------------- helpers */

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn matches_cidr(bytes: &[u8; 4], base: &str, bits: u32) -> bool {
    let Some(base_bytes) = parse_ipv4(base) else {
        return false;
    };
    for (i, item) in bytes.iter().enumerate() {
        let remaining = bits as i32 - (i as i32) * 8;
        if remaining <= 0 {
            break;
        }
        let mask: u8 = if remaining >= 8 {
            0xff
        } else {
            (0xffu32 << (8 - remaining)) as u8 & 0xff
        };
        if (item & mask) != (base_bytes[i] & mask) {
            return false;
        }
    }
    true
}

/* ------------------------------------------------------------------ IPv4 */

/// Strict dotted-quad parsing.
///
/// Leading zeros are rejected (`01.2.3.4`): different parsers disagree on
/// whether they mean an octal literal, so accepting them would make this API
/// return a different address than the caller's own tooling does.
pub fn parse_ipv4(input: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = input.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut bytes = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        // ^\d{1,3}$
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        let value: u32 = part.parse().ok()?;
        if value > 255 {
            return None;
        }
        bytes[i] = value as u8;
    }
    Some(bytes)
}

/* ------------------------------------------------------------------ IPv6 */

fn is_ipv6_group(token: &str) -> bool {
    (1..=4).contains(&token.len())
        && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Expands one side of a (possibly compressed) IPv6 address into raw bytes.
///
/// The embedded-IPv4 form (`::ffff:192.0.2.128`) is only legal as the very
/// last token of the whole address, which is why `is_last` is threaded through.
fn expand_segment(segment: &str, is_last: bool) -> Option<Vec<u8>> {
    if segment.is_empty() {
        return Some(Vec::new());
    }
    let tokens: Vec<&str> = segment.split(':').collect();
    let mut bytes: Vec<u8> = Vec::new();

    for (i, token) in tokens.iter().enumerate() {
        if token.is_empty() {
            return None;
        }
        let last_token = is_last && i == tokens.len() - 1;
        if token.contains('.') {
            if !last_token {
                return None;
            }
            let embedded = parse_ipv4(token)?;
            bytes.extend_from_slice(&embedded);
            continue;
        }
        if !is_ipv6_group(token) {
            return None;
        }
        let value = u16::from_str_radix(token, 16).ok()?;
        bytes.push((value >> 8) as u8);
        bytes.push((value & 0xff) as u8);
    }
    Some(bytes)
}

pub fn parse_ipv6(input: &str) -> Option<[u8; 16]> {
    if input.len() < 2 || input.len() > 45 {
        return None;
    }
    // A zone id (`fe80::1%eth0`) is a link-local, node-local construct: it is
    // not part of the address and cannot be resolved by anyone else.
    if input.contains('%') {
        return None;
    }
    // Guards the overlapping-match case that a naive split would otherwise miss.
    if input.contains(":::") {
        return None;
    }
    if !input.bytes().all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.') {
        return None;
    }

    let halves: Vec<&str> = input.split("::").collect();
    if halves.len() > 2 {
        return None;
    }

    let head = expand_segment(halves[0], halves.len() == 1)?;
    let head_groups = head.len() / 2;

    let tail: Vec<u8>;
    if halves.len() == 2 {
        let tail_segment = expand_segment(halves[1], true)?;
        // `::` must stand for at least one elided group of zeros.
        if head_groups + tail_segment.len() / 2 >= 8 {
            return None;
        }
        tail = tail_segment;
    } else {
        if head_groups != 8 {
            return None;
        }
        tail = Vec::new();
    }

    let mut bytes = [0u8; 16];
    bytes[..head.len()].copy_from_slice(&head);
    let tail_start = 16 - tail.len();
    bytes[tail_start..].copy_from_slice(&tail);
    Some(bytes)
}

/* -------------------------------------------------------- classification */

const IPV4_RULES: &[(&str, &str, u32)] = &[
    (SCOPE_UNSPECIFIED, "0.0.0.0", 8),
    (SCOPE_LOOPBACK, "127.0.0.0", 8),
    (SCOPE_PRIVATE, "10.0.0.0", 8),
    (SCOPE_PRIVATE, "172.16.0.0", 12),
    (SCOPE_PRIVATE, "192.168.0.0", 16),
    (SCOPE_CARRIER_GRADE_NAT, "100.64.0.0", 10),
    (SCOPE_LINK_LOCAL, "169.254.0.0", 16),
    (SCOPE_PRIVATE, "192.0.0.0", 24),
    (SCOPE_RESERVED, "192.0.2.0", 24),
    (SCOPE_RESERVED, "198.18.0.0", 15),
    (SCOPE_RESERVED, "198.51.100.0", 24),
    (SCOPE_RESERVED, "203.0.113.0", 24),
    (SCOPE_MULTICAST, "224.0.0.0", 4),
    (SCOPE_RESERVED, "240.0.0.0", 4),
];

pub fn classify_ipv4(bytes: &[u8; 4]) -> &'static str {
    if bytes == &[255, 255, 255, 255] {
        return SCOPE_BROADCAST;
    }
    for (scope, base, bits) in IPV4_RULES {
        if matches_cidr(bytes, base, *bits) {
            return scope;
        }
    }
    SCOPE_PUBLIC
}

pub fn classify_ipv6(bytes: &[u8; 16]) -> &'static str {
    if bytes.iter().all(|b| *b == 0) {
        return SCOPE_UNSPECIFIED;
    }
    // ::1
    if bytes[..15].iter().all(|b| *b == 0) && bytes[15] == 1 {
        return SCOPE_LOOPBACK;
    }
    // ff00::/8
    if bytes[0] == 0xff {
        return SCOPE_MULTICAST;
    }
    // fe80::/10
    if bytes[0] == 0xfe && (bytes[1] & 0xc0) == 0x80 {
        return SCOPE_LINK_LOCAL;
    }
    // fc00::/7 unique local
    if (bytes[0] & 0xfe) == 0xfc {
        return SCOPE_PRIVATE;
    }
    // 2001:db8::/32 documentation
    if bytes[0] == 0x20 && bytes[1] == 0x01 && bytes[2] == 0x0d && bytes[3] == 0xb8 {
        return SCOPE_RESERVED;
    }
    // 100::/64 discard-only
    if bytes[0] == 0x01 && bytes[1] == 0x00 && bytes[2..8].iter().all(|b| *b == 0) {
        return SCOPE_RESERVED;
    }
    // 2001::/23 IETF protocol assignments
    if bytes[0] == 0x20 && bytes[1] == 0x01 && (bytes[2] & 0xfe) == 0x00 {
        return SCOPE_RESERVED;
    }
    SCOPE_PUBLIC
}

/* ------------------------------------------------------------ formatting */

/// RFC 5952 canonical text form: lowercase hex, no leading zeros, and the
/// longest run of two or more zero groups replaced by `::` (leftmost run wins).
pub fn format_ipv6(bytes: &[u8; 16]) -> String {
    let groups: Vec<u16> = (0..16)
        .step_by(2)
        .map(|i| ((bytes[i] as u16) << 8) | bytes[i + 1] as u16)
        .collect();

    // RFC 5952 §4.2: shorten the longest run of two or more zero groups; on a
    // tie the leftmost run wins (§4.2.2).
    let mut best_start: i32 = -1;
    let mut best_length = 0usize;
    let mut run_start: i32 = -1;
    let mut run_length = 0usize;
    for (i, group) in groups.iter().enumerate() {
        if *group == 0 {
            if run_start == -1 {
                run_start = i as i32;
            }
            run_length += 1;
            continue;
        }
        if run_start != -1 && run_length >= 2 && run_length > best_length {
            best_length = run_length;
            best_start = run_start;
        }
        run_start = -1;
        run_length = 0;
    }
    // A run that reaches the end of the address is only closed by the loop
    // above when a non-zero group follows; check it once more.
    if run_start != -1 && run_length >= 2 && run_length > best_length {
        best_length = run_length;
        best_start = run_start;
    }

    let render = |slice: &[u16]| -> String {
        slice
            .iter()
            .map(|g| format!("{g:x}"))
            .collect::<Vec<_>>()
            .join(":")
    };

    if best_start == -1 {
        return render(&groups);
    }
    let start = best_start as usize;
    let head = render(&groups[..start]);
    let tail = render(&groups[start + best_length..]);
    format!("{head}::{tail}")
}

/* ------------------------------------------------------------ public API */

/// Parses `input` into a canonical address, or returns `None` when the text is
/// not a valid IPv4 or IPv6 address. Leading/trailing whitespace is ignored
/// because it routinely arrives from a hand-typed query string.
pub fn parse_ip_address(input: &str) -> Option<ParsedIpAddress> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }

    if value.contains(':') {
        let bytes = parse_ipv6(value)?;
        let scope = classify_ipv6(&bytes);
        return Some(ParsedIpAddress {
            version: 6,
            address: format_ipv6(&bytes),
            hex: to_hex(&bytes),
            scope,
            is_public: scope == SCOPE_PUBLIC,
        });
    }

    let bytes = parse_ipv4(value)?;
    let scope = classify_ipv4(&bytes);
    Some(ParsedIpAddress {
        version: 4,
        address: bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join("."),
        hex: to_hex(&bytes),
        scope,
        is_public: scope == SCOPE_PUBLIC,
    })
}

/// Explains, in the caller's own words, why a piece of text was rejected. Kept
/// next to the parser so the message can never drift from the real rule set.
pub fn describe_rejection(input: &str) -> String {
    let value = input.trim();
    if value.is_empty() {
        return "the address is empty".to_string();
    }
    if value.contains('%') {
        return "IPv6 zone identifiers are not accepted; remove the \"%zone\" suffix".to_string();
    }
    if value.contains('/') {
        return "CIDR ranges are not accepted; supply a single address".to_string();
    }
    if value.contains(':') {
        return "expected 8 hex groups, or fewer with one \"::\"".to_string();
    }
    let octets: Vec<&str> = value.split('.').collect();
    if octets.len() != 4 {
        return format!("expected 4 dot-separated octets, received {}", octets.len());
    }
    if octets.iter().any(|o| o.len() > 1 && o.starts_with('0')) {
        return "leading zeros are not allowed in an octet".to_string();
    }
    if octets
        .iter()
        .any(|o| o.is_empty() || o.len() > 3 || !o.bytes().all(|b| b.is_ascii_digit()))
    {
        return "each octet must be one to three decimal digits".to_string();
    }
    "each octet must be between 0 and 255".to_string()
}

/* ------------------------------------------------------- domain failures */

/// 400 — the supplied text is not a syntactically valid IPv4/IPv6 address.
pub fn invalid_address_error(input: &str, reason: String) -> ApiError {
    ApiError::validation_details(
        "INVALID_ADDRESS",
        "the address is not a valid IPv4 or IPv6 address",
        json!({ "address": input, "reason": reason }),
    )
}

/// 400 — the address parses but is not globally routable; never calls upstream.
pub fn non_public_address_error(parsed: &ParsedIpAddress) -> ApiError {
    ApiError::validation_details(
        "NON_PUBLIC_ADDRESS",
        format!(
            "{} is a {} address and cannot be geolocated",
            parsed.address, parsed.scope
        ),
        json!({
            "address": parsed.address,
            "ipVersion": parsed.version,
            "scope": parsed.scope,
        }),
    )
}

/// Parses or raises the exact `INVALID_ADDRESS` 400 the original service raised.
pub fn parse_or_throw(input: &str) -> Result<ParsedIpAddress, ApiError> {
    parse_ip_address(input).ok_or_else(|| invalid_address_error(input, describe_rejection(input)))
}

/// Rejects non-public addresses before any upstream call is attempted.
pub fn assert_public(parsed: &ParsedIpAddress) -> Result<(), ApiError> {
    if !parsed.is_public {
        return Err(non_public_address_error(parsed));
    }
    Ok(())
}

/* --------------------------------------------- caller-address resolution */

/// A single entry of the `GEOLOCATION__TRUSTED_PROXIES` setting.
///
/// `*` trusts every hop (only safe behind a proxy that overwrites the header
/// itself); anything else is an exact address or a CIDR block.
struct TrustedProxyRule {
    all: bool,
    bytes: Option<Vec<u8>>,
    prefix_bits: u32,
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("00"), 16).unwrap_or(0))
        .collect()
}

fn parse_trusted_proxies(raw: &str) -> Result<Vec<TrustedProxyRule>, ApiError> {
    let mut rules = Vec::new();
    for part in raw.split(',') {
        let entry = part.trim();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            rules.push(TrustedProxyRule { all: true, bytes: None, prefix_bits: 0 });
            continue;
        }
        let (network, bits_text) = match entry.split_once('/') {
            Some((n, b)) => (n, Some(b)),
            None => (entry, None),
        };
        let parsed = parse_ip_address(network.trim()).ok_or_else(|| {
            ApiError::validation_details(
                "INVALID_REQUEST",
                format!(
                    "GEOLOCATION__TRUSTED_PROXIES contains an entry that is not an IP address or CIDR block: \"{entry}\""
                ),
                json!({ "entry": entry }),
            )
        })?;
        let width: u32 = if parsed.version == 4 { 32 } else { 128 };
        let prefix_bits = match bits_text {
            None => width,
            Some(bits) => bits.trim().parse::<u32>().map_err(|_| {
                ApiError::validation_details(
                    "INVALID_REQUEST",
                    format!(
                        "GEOLOCATION__TRUSTED_PROXIES contains an invalid prefix length: \"{entry}\""
                    ),
                    json!({ "entry": entry }),
                )
            })?,
        };
        if prefix_bits > width {
            return Err(ApiError::validation_details(
                "INVALID_REQUEST",
                format!(
                    "GEOLOCATION__TRUSTED_PROXIES contains an invalid prefix length: \"{entry}\""
                ),
                json!({ "entry": entry }),
            ));
        }
        rules.push(TrustedProxyRule {
            all: false,
            bytes: Some(hex_to_bytes(&parsed.hex)),
            prefix_bits,
        });
    }
    Ok(rules)
}

fn in_network(hex: &str, rule: &TrustedProxyRule) -> bool {
    if rule.all {
        return true;
    }
    let Some(rule_bytes) = &rule.bytes else {
        return false;
    };
    let bytes = hex_to_bytes(hex);
    if bytes.len() != rule_bytes.len() {
        return false;
    }
    for (i, item) in bytes.iter().enumerate() {
        let remaining = rule.prefix_bits as i32 - (i as i32) * 8;
        if remaining <= 0 {
            break;
        }
        let mask: u8 = if remaining >= 8 {
            0xff
        } else {
            (0xffu32 << (8 - remaining)) as u8 & 0xff
        };
        if (item & mask) != (rule_bytes[i] & mask) {
            return false;
        }
    }
    true
}

/// The address the service believes belongs to the caller, plus whether a
/// forwarded header was believed.
pub struct ClientAddress {
    pub address: String,
    pub forwarded: bool,
}

/// Works out the caller's public address from transport + headers.
///
/// `X-Forwarded-For` is client-controllable, so it is only believed when the
/// transport peer is an explicitly trusted proxy. The chain is walked from the
/// right (nearest hop) to the left and the first address outside the trusted
/// set wins, which is the address the outermost trusted proxy observed.
pub fn resolve_client_address(
    peer: Option<&str>,
    x_forwarded_for: Option<&str>,
    x_real_ip: Option<&str>,
    trusted_proxies: &str,
) -> Result<ClientAddress, ApiError> {
    let rules = parse_trusted_proxies(trusted_proxies)?;
    // Express reports IPv4-mapped IPv6 peers as `::ffff:a.b.c.d`; strip it so
    // the address compares equal to its plain IPv4 form.
    let peer = peer.map(|p| p.strip_prefix("::ffff:").unwrap_or(p));
    let parsed_peer = peer.and_then(parse_ip_address);
    let peer_address = parsed_peer
        .as_ref()
        .map(|p| p.address.clone())
        .or_else(|| peer.map(|p| p.to_string()))
        .unwrap_or_default();

    let peer_is_trusted = parsed_peer
        .as_ref()
        .map(|p| rules.iter().any(|rule| in_network(&p.hex, rule)))
        .unwrap_or(false);

    if !peer_is_trusted {
        return Ok(ClientAddress { address: peer_address, forwarded: false });
    }

    let chain: Vec<&str> = x_forwarded_for
        .unwrap_or("")
        .split(',')
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .collect();

    // `*` means the deployment guarantees the header is written by its own
    // edge, so the leftmost entry is the client and no hop filtering applies.
    if rules.iter().any(|rule| rule.all) {
        for candidate in &chain {
            if let Some(parsed) = parse_ip_address(candidate) {
                return Ok(ClientAddress { address: parsed.address, forwarded: true });
            }
        }
    } else {
        // Otherwise walk from the nearest hop outward and take the first
        // address that is not itself a trusted proxy.
        for candidate in chain.iter().rev() {
            let Some(parsed) = parse_ip_address(candidate) else { continue };
            if rules.iter().any(|rule| in_network(&parsed.hex, rule)) {
                continue;
            }
            return Ok(ClientAddress { address: parsed.address, forwarded: true });
        }
    }

    if let Some(real_ip) = x_real_ip {
        if let Some(parsed) = parse_ip_address(real_ip) {
            if !rules.iter().any(|rule| in_network(&parsed.hex, rule)) {
                return Ok(ClientAddress { address: parsed.address, forwarded: true });
            }
        }
    }

    Ok(ClientAddress { address: peer_address, forwarded: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strict_ipv4_and_rejects_loose_forms() {
        let parsed = parse_ip_address(" 8.8.8.8 ").expect("public address");
        assert_eq!(parsed.version, 4);
        assert_eq!(parsed.address, "8.8.8.8");
        assert_eq!(parsed.hex, "08080808");
        assert_eq!(parsed.scope, "public");
        assert!(parsed.is_public);

        assert!(parse_ip_address("01.2.3.4").is_none(), "leading zero");
        assert!(parse_ip_address("256.1.1.1").is_none(), "octet overflow");
        assert!(parse_ip_address("1.2.3").is_none(), "three octets");
        assert!(parse_ip_address("1.2.3.4/32").is_none(), "CIDR");
        assert!(parse_ip_address("").is_none(), "empty");
    }

    #[test]
    fn parses_canonical_ipv6() {
        let parsed = parse_ip_address("2001:0DB8:0000:0000:0000:0000:0000:0001").expect("v6");
        assert_eq!(parsed.version, 6);
        assert_eq!(parsed.address, "2001:db8::1");
        assert_eq!(parsed.hex, "20010db8000000000000000000000001");
        assert_eq!(parsed.scope, "reserved");

        let mapped = parse_ip_address("::ffff:192.0.2.128").expect("embedded v4");
        assert_eq!(mapped.hex, "00000000000000000000ffffc0000280");
        assert_eq!(mapped.address, "::ffff:c000:280");

        assert!(parse_ip_address("fe80::1%eth0").is_none(), "zone id");
        assert!(parse_ip_address("1:2:3:4:5:6:7:8:9").is_none(), "too long");
        assert!(parse_ip_address("1:2:3:4:5:6:7:8::1").is_none(), "double compression");
        assert!(parse_ip_address("12345::").is_none(), "group too long");
    }

    #[test]
    fn classifies_ipv4_scopes() {
        assert_eq!(classify_ipv4(&[127, 0, 0, 1]), "loopback");
        assert_eq!(classify_ipv4(&[10, 1, 2, 3]), "private");
        assert_eq!(classify_ipv4(&[172, 16, 5, 5]), "private");
        assert_eq!(classify_ipv4(&[172, 32, 5, 5]), "public");
        assert_eq!(classify_ipv4(&[192, 168, 0, 9]), "private");
        assert_eq!(classify_ipv4(&[100, 64, 0, 1]), "carrier-grade-nat");
        assert_eq!(classify_ipv4(&[169, 254, 9, 9]), "link-local");
        assert_eq!(classify_ipv4(&[224, 0, 0, 1]), "multicast");
        assert_eq!(classify_ipv4(&[255, 255, 255, 255]), "broadcast");
        assert_eq!(classify_ipv4(&[203, 0, 113, 7]), "reserved");
    }

    #[test]
    fn classifies_ipv6_scopes() {
        assert_eq!(classify_ipv6(&[0; 16]), "unspecified");
        let mut loopback = [0u8; 16];
        loopback[15] = 1;
        assert_eq!(classify_ipv6(&loopback), "loopback");
        let mut ll = [0u8; 16];
        ll[0] = 0xfe;
        ll[1] = 0x80;
        assert_eq!(classify_ipv6(&ll), "link-local");
        let mut ula = [0u8; 16];
        ula[0] = 0xfc;
        assert_eq!(classify_ipv6(&ula), "private");
        let mut mc = [0u8; 16];
        mc[0] = 0xff;
        assert_eq!(classify_ipv6(&mc), "multicast");
        assert_eq!(parse_ip_address("2001:db8::1").unwrap().scope, "reserved");
        assert_eq!(parse_ip_address("2606:4700::1").unwrap().scope, "public");
    }

    #[test]
    fn describes_rejections_in_caller_words() {
        assert_eq!(describe_rejection("   "), "the address is empty");
        assert_eq!(
            describe_rejection("fe80::1%eth0"),
            "IPv6 zone identifiers are not accepted; remove the \"%zone\" suffix"
        );
        assert_eq!(
            describe_rejection("10.0.0.0/8"),
            "CIDR ranges are not accepted; supply a single address"
        );
        assert_eq!(
            describe_rejection("1:2"),
            "expected 8 hex groups, or fewer with one \"::\""
        );
        assert_eq!(describe_rejection("1.2.3"), "expected 4 dot-separated octets, received 3");
        assert_eq!(describe_rejection("01.2.3.4"), "leading zeros are not allowed in an octet");
        assert_eq!(
            describe_rejection("a.b.c.d"),
            "each octet must be one to three decimal digits"
        );
        assert_eq!(describe_rejection("1.2.3.999"), "each octet must be between 0 and 255");
    }

    #[test]
    fn invalid_and_non_public_errors_carry_original_codes() {
        let err = parse_or_throw("not-an-ip").unwrap_err();
        assert_eq!(err.to_string(), "the address is not a valid IPv4 or IPv6 address");

        let err = assert_public(&parse_or_throw("127.0.0.1").unwrap()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "127.0.0.1 is a loopback address and cannot be geolocated"
        );

        assert!(assert_public(&parse_or_throw("8.8.8.8").unwrap()).is_ok());
    }

    #[test]
    fn client_address_ignores_forwarded_header_from_untrusted_peer() {
        let resolved = resolve_client_address(
            Some("127.0.0.1"),
            Some("203.0.113.7, 8.8.8.8"),
            None,
            "",
        )
        .unwrap();
        assert_eq!(resolved.address, "127.0.0.1");
        assert!(!resolved.forwarded);
    }

    #[test]
    fn client_address_walks_chain_when_peer_is_trusted() {
        // 203.0.113.7 is inside the trusted proxy set (TEST-NET-3), so the
        // first address outside it wins.
        let resolved = resolve_client_address(
            Some("203.0.113.7"),
            Some("203.0.113.7, 198.51.100.9, 8.8.8.8"),
            None,
            "203.0.113.0/24",
        )
        .unwrap();
        assert_eq!(resolved.address, "198.51.100.9");
        assert!(resolved.forwarded);
    }

    #[test]
    fn client_address_falls_back_to_x_real_ip_then_peer() {
        let resolved = resolve_client_address(
            Some("203.0.113.7"),
            None,
            Some("8.8.4.4"),
            "203.0.113.0/24",
        )
        .unwrap();
        assert_eq!(resolved.address, "8.8.4.4");
        assert!(resolved.forwarded);

        let resolved =
            resolve_client_address(Some("203.0.113.7"), None, None, "203.0.113.0/24").unwrap();
        assert_eq!(resolved.address, "203.0.113.7");
        assert!(!resolved.forwarded);
    }

    #[test]
    fn client_address_strips_ipv4_mapped_peer() {
        let resolved = resolve_client_address(Some("::ffff:127.0.0.1"), None, None, "").unwrap();
        assert_eq!(resolved.address, "127.0.0.1");
        assert!(!resolved.forwarded);
    }

    #[test]
    fn client_address_rejects_a_malformed_proxy_configuration() {
        let err =
            resolve_client_address(Some("127.0.0.1"), None, None, "not-an-ip").unwrap_err();
        assert!(err.to_string().contains("not an IP address or CIDR block"));
    }
}
