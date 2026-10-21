//! ip-geolocation service — IP address intelligence over ipwho.is, with local
//! address-facts computation (no upstream needed for `/validate`).
//!
//! Ported from `zoofam26/ip-geolocation-api` (NestJS).

mod routes;

use actix_web::web;
use utoipa::openapi::OpenApi;

pub const SERVICE_PREFIX: &str = "/ip-geolocation";
pub const CODE_PREFIX: &str = "IPGEO";
pub const UPSTREAM_IPWHOIS: &str = "https://ipwho.is";

/// Cache TTL (s) — geolocation records change slowly.
pub const TTL_SECS: u64 = 3_600;

/// Fields accepted by the `fields` query parameter, in catalogue order.
pub const ALLOWED_FIELDS: &[&str] = &[
    "continent", "continentCode", "country", "countryCode", "region", "regionCode", "city",
    "postal", "latitude", "longitude", "timezone", "timezoneAbbreviation", "timezoneOffset",
    "isDst", "callingCode", "capital", "borders", "isEu", "flag", "asn", "organization", "isp",
    "domain",
];

pub fn router(cfg: &mut web::ServiceConfig) {
    routes::configure(cfg);
}

pub fn openapi() -> OpenApi {
    routes::openapi_doc()
}

pub fn index() -> serde_json::Value {
    serde_json::json!({
        "name": "ip-geolocation-api",
        "prefix": SERVICE_PREFIX,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "IP address intelligence: geolocation over the keyless ipwho.is provider, ASN/ISP details, field projection and local address classification (loopback/private/multicast/...).",
        "dataSource": {
            "provider": "ipwho.is",
            "upstream": [UPSTREAM_IPWHOIS],
            "requiresApiKey": false,
            "note": "Geolocation records are cached for an hour; address facts (/validate) are computed locally from the parsed address.",
        },
        "endpoints": [
            { "method": "GET", "path": "/ip-geolocation/ip", "description": "Look up the caller's address (X-Forwarded-For aware) with field projection" },
            { "method": "GET", "path": "/ip-geolocation/ip/{address}", "description": "Full geolocation record for one address, with field projection" },
            { "method": "GET", "path": "/ip-geolocation/geo/{address}", "description": "Geographic subset for one address" },
            { "method": "GET", "path": "/ip-geolocation/asn/{address}", "description": "ASN/ISP details for one address" },
            { "method": "GET", "path": "/ip-geolocation/validate/{address}", "description": "Local address facts: version, scope, loopback/private/multicast flags" },
            { "method": "GET", "path": "/ip-geolocation/health", "description": "Liveness plus a real upstream probe" },
            { "method": "GET", "path": "/ip-geolocation/health/ready", "description": "Readiness probe" },
            { "method": "GET", "path": "/ip-geolocation/openapi.json", "description": "OpenAPI 3 document for this service" },
        ],
    })
}

/// Address classification, computed locally.
pub struct AddressFacts {
    pub canonical: String,
    pub hex: String,
    pub ip_version: &'static str,
    pub scope: &'static str,
    pub is_public: bool,
    pub is_loopback: bool,
    pub is_private: bool,
    pub is_link_local: bool,
    pub is_multicast: bool,
    pub is_unspecified: bool,
    pub is_broadcast: bool,
    pub is_carrier_grade_nat: bool,
    pub is_reserved: bool,
}

pub fn classify_address(addr: &std::net::IpAddr) -> AddressFacts {
    use std::net::IpAddr::{V4, V6};
    let (ip_version, hex, scope, is_public, is_loopback, is_private, is_link_local, is_multicast, is_unspecified, is_broadcast, is_cgnat, is_reserved) = match addr {
        V4(v4) => {
            let octets = v4.octets();
            let is_private = v4.is_private();
            let is_loopback = v4.is_loopback();
            let is_link_local = v4.is_link_local();
            let is_multicast = v4.is_multicast();
            let is_unspecified = v4.is_unspecified();
            let is_broadcast = octets == [255, 255, 255, 255];
            let is_cgnat = octets[0] == 100 && (64..=127).contains(&octets[1]);
            let is_reserved = v4.is_broadcast() || v4.is_documentation() || is_broadcast
                || octets[0] == 0 || octets[0] == 127
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
                || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
                || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
                || octets[0] >= 240;
            let scope = if is_loopback {
                "loopback"
            } else if is_private {
                "private"
            } else if is_cgnat {
                "carrier-grade-nat"
            } else if is_link_local {
                "link-local"
            } else if is_multicast {
                "multicast"
            } else if is_unspecified || is_broadcast || is_reserved {
                "reserved"
            } else {
                "public"
            };
            let is_public = scope == "public";
            let hex = octets.iter().map(|b| format!("{b:02X}")).collect::<String>();
            ("IPv4", hex, scope, is_public, is_loopback, is_private, is_link_local, is_multicast, is_unspecified, is_broadcast, is_cgnat, v4.is_documentation() || is_reserved)
        }
        V6(v6) => {
            let segments = v6.segments();
            let is_loopback = v6.is_loopback();
            let is_unspecified = v6.is_unspecified();
            let is_multicast = v6.is_multicast();
            let is_link_local = (segments[0] & 0xffc0) == 0xfe80;
            let is_unique_local = (segments[0] & 0xfe00) == 0xfc00;
            let is_doc = (segments[0] == 0x2001) && (segments[1] == 0x0db8);
            let scope = if is_loopback {
                "loopback"
            } else if is_unspecified {
                "reserved"
            } else if is_multicast {
                "multicast"
            } else if is_link_local {
                "link-local"
            } else if is_unique_local {
                "private"
            } else if is_doc {
                "reserved"
            } else {
                "public"
            };
            let is_public = scope == "public";
            let is_reserved = is_unspecified || is_doc || (segments[0] & 0xff00) == 0xff00;
            let hex = segments.iter().map(|s| format!("{s:04X}")).collect::<String>();
            ("IPv6", hex, scope, is_public, is_loopback, is_unique_local, is_link_local, is_multicast, is_unspecified, false, false, is_reserved)
        }
    };
    AddressFacts {
        canonical: addr.to_string(),
        hex,
        ip_version,
        scope,
        is_public,
        is_loopback,
        is_private,
        is_link_local,
        is_multicast,
        is_unspecified,
        is_broadcast,
        is_carrier_grade_nat: is_cgnat,
        is_reserved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn facts(s: &str) -> AddressFacts {
        classify_address(&s.parse::<IpAddr>().unwrap())
    }

    #[test]
    fn classifies_public_v4() {
        let f = facts("8.8.8.8");
        assert_eq!(f.scope, "public");
        assert!(f.is_public);
        assert_eq!(f.ip_version, "IPv4");
        assert_eq!(f.hex, "08080808");
    }

    #[test]
    fn classifies_private_loopback_cgnat() {
        assert_eq!(facts("192.168.1.1").scope, "private");
        assert_eq!(facts("127.0.0.1").scope, "loopback");
        assert_eq!(facts("100.64.0.1").scope, "carrier-grade-nat");
        assert_eq!(facts("169.254.1.1").scope, "link-local");
        assert_eq!(facts("224.0.0.1").scope, "multicast");
        assert_eq!(facts("255.255.255.255").scope, "reserved");
        assert!(facts("255.255.255.255").is_broadcast);
    }

    #[test]
    fn classifies_v6() {
        assert_eq!(facts("::1").scope, "loopback");
        assert_eq!(facts("2001:db8::1").scope, "reserved");
        assert_eq!(facts("2606:4700::1").scope, "public");
        assert!(facts("2606:4700::1").ip_version == "IPv6");
    }
}
