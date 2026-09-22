//! Connection parameters and libpq-compatible connection-string parsing.
//!
//! Accepts both `postgresql://user:pw@h1:5432,h2/db?sslmode=require` URIs and
//! `host=h1 port=5432 dbname='my db'` keyword/value strings.

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    Allow,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    fn parse(value: &str) -> Result<Self, ParseError> {
        Ok(match value {
            "disable" => Self::Disable,
            "allow" => Self::Allow,
            "prefer" => Self::Prefer,
            "require" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" => Self::VerifyFull,
            _ => return Err(ParseError::invalid("sslmode", value)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelBinding {
    Disable,
    #[default]
    Prefer,
    Require,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetSessionAttrs {
    #[default]
    Any,
    ReadWrite,
    ReadOnly,
    Primary,
    Standby,
    PreferStandby,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpec {
    /// DNS name, IP address, or (when it starts with `/`) a Unix socket directory.
    pub host: String,
    pub port: Option<u16>,
}

impl HostSpec {
    pub fn is_unix_socket(&self) -> bool {
        self.host.starts_with('/')
    }
}

/// Everything needed to open a session, minus the SSH tunnel (see the `tunnel` module).
/// The password is intentionally kept out of the serialized profile; it lives in the Keychain.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionParams {
    pub hosts: Vec<HostSpec>,
    pub user: Option<String>,
    #[serde(skip)]
    pub password: Option<String>,
    pub dbname: Option<String>,
    pub passfile: Option<String>,
    pub service: Option<String>,

    pub sslmode: SslMode,
    pub sslrootcert: Option<String>,
    pub sslcert: Option<String>,
    pub sslkey: Option<String>,
    pub sslcrl: Option<String>,
    pub ssl_direct_negotiation: bool,

    pub connect_timeout: Option<Duration>,
    pub application_name: Option<String>,
    pub options: Option<String>,
    pub channel_binding: ChannelBinding,
    pub target_session_attrs: TargetSessionAttrs,
    pub load_balance_random: bool,

    pub keepalives: Option<bool>,
    pub keepalives_idle: Option<Duration>,
    pub keepalives_interval: Option<Duration>,
    pub keepalives_retries: Option<u32>,
    pub tcp_user_timeout: Option<Duration>,
}

/// Result of parsing a connection string.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedConnInfo {
    pub params: ConnectionParams,
    /// Valid libpq keywords we recognise but do not implement (e.g. `gssencmode`), so the UI
    /// can tell the user they were ignored instead of silently dropping them.
    pub unsupported: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("invalid value {value:?} for `{key}`")]
    InvalidValue { key: String, value: String },
    #[error("unknown connection parameter `{0}`")]
    UnknownParameter(String),
    #[error("malformed connection string: {0}")]
    Malformed(String),
    #[error("`host` and `port` lists differ in length ({hosts} hosts, {ports} ports)")]
    HostPortMismatch { hosts: usize, ports: usize },
}

impl ParseError {
    fn invalid(key: &str, value: &str) -> Self {
        Self::InvalidValue {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// libpq keywords that are valid but not implemented in this app.
const UNSUPPORTED_KEYS: &[&str] = &[
    "gssencmode",
    "krbsrvname",
    "gsslib",
    "gssdelegation",
    "replication",
    "require_auth",
    "scram_client_key",
    "scram_server_key",
    "min_protocol_version",
    "max_protocol_version",
    "oauth_issuer",
    "oauth_client_id",
    "oauth_client_secret",
    "oauth_scope",
    "sslkeylogfile",
    "sslcertmode",
    "sslcompression",
    "sslsni",
    "sslpassword",
    "ssl_min_protocol_version",
    "ssl_max_protocol_version",
    "sslcrldir",
    "requirepeer",
    "client_encoding",
    "fallback_application_name",
];

pub fn parse_conninfo(input: &str) -> Result<ParsedConnInfo, ParseError> {
    let input = input.trim();
    if input.starts_with("postgresql://") || input.starts_with("postgres://") {
        parse_uri(input)
    } else {
        parse_keyword_value(input)
    }
}

/// Accumulates raw values so multi-host lists can be validated once at the end.
#[derive(Default)]
struct Builder {
    info: ParsedConnInfo,
    hosts: Vec<String>,
    ports: Vec<String>,
    hostaddrs: Vec<String>,
}

impl Builder {
    fn set(&mut self, key: &str, value: &str) -> Result<(), ParseError> {
        let p = &mut self.info.params;
        let some = |v: &str| Some(v.to_string());
        let secs = |v: &str| {
            v.parse::<u64>()
                .map(Duration::from_secs)
                .map_err(|_| ParseError::invalid(key, v))
        };
        let flag = |v: &str| match v {
            "1" | "true" | "on" | "yes" => Ok(true),
            "0" | "false" | "off" | "no" => Ok(false),
            _ => Err(ParseError::invalid(key, v)),
        };
        match key {
            "host" => self.hosts = split_list(value),
            "hostaddr" => self.hostaddrs = split_list(value),
            "port" => self.ports = split_list(value),
            "user" => p.user = some(value),
            "password" => p.password = some(value),
            "dbname" => p.dbname = some(value),
            "passfile" => p.passfile = some(value),
            "service" => p.service = some(value),
            "sslmode" => p.sslmode = SslMode::parse(value)?,
            "sslrootcert" => p.sslrootcert = some(value),
            "sslcert" => p.sslcert = some(value),
            "sslkey" => p.sslkey = some(value),
            "sslcrl" => p.sslcrl = some(value),
            "sslnegotiation" => {
                p.ssl_direct_negotiation = match value {
                    "postgres" => false,
                    "direct" => true,
                    _ => return Err(ParseError::invalid(key, value)),
                }
            }
            "connect_timeout" => {
                // libpq: 0 (or negative) means wait indefinitely.
                let d = secs(value)?;
                p.connect_timeout = (!d.is_zero()).then_some(d);
            }
            "application_name" => p.application_name = some(value),
            "options" => p.options = some(value),
            "channel_binding" => {
                p.channel_binding = match value {
                    "disable" => ChannelBinding::Disable,
                    "prefer" => ChannelBinding::Prefer,
                    "require" => ChannelBinding::Require,
                    _ => return Err(ParseError::invalid(key, value)),
                }
            }
            "target_session_attrs" => {
                p.target_session_attrs = match value {
                    "any" => TargetSessionAttrs::Any,
                    "read-write" => TargetSessionAttrs::ReadWrite,
                    "read-only" => TargetSessionAttrs::ReadOnly,
                    "primary" => TargetSessionAttrs::Primary,
                    "standby" => TargetSessionAttrs::Standby,
                    "prefer-standby" => TargetSessionAttrs::PreferStandby,
                    _ => return Err(ParseError::invalid(key, value)),
                }
            }
            "load_balance_hosts" => {
                p.load_balance_random = match value {
                    "disable" => false,
                    "random" => true,
                    _ => return Err(ParseError::invalid(key, value)),
                }
            }
            "keepalives" => p.keepalives = Some(flag(value)?),
            "keepalives_idle" => p.keepalives_idle = Some(secs(value)?),
            "keepalives_interval" => p.keepalives_interval = Some(secs(value)?),
            "keepalives_count" | "keepalives_retries" => {
                p.keepalives_retries =
                    Some(value.parse().map_err(|_| ParseError::invalid(key, value))?)
            }
            "tcp_user_timeout" => {
                let ms: u64 = value.parse().map_err(|_| ParseError::invalid(key, value))?;
                p.tcp_user_timeout = Some(Duration::from_millis(ms));
            }
            k if UNSUPPORTED_KEYS.contains(&k) => {
                self.info.unsupported.insert(k.into(), value.into());
            }
            k => return Err(ParseError::UnknownParameter(k.into())),
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ParsedConnInfo, ParseError> {
        // `hostaddr` only replaces DNS resolution, so it is treated as the host list when
        // no `host` was given; a real host name + hostaddr pairing is not needed by this app.
        let hosts = if self.hosts.is_empty() {
            std::mem::take(&mut self.hostaddrs)
        } else {
            std::mem::take(&mut self.hosts)
        };
        let ports: Vec<Option<u16>> = self
            .ports
            .iter()
            .map(|p| {
                p.parse::<u16>()
                    .map(Some)
                    .map_err(|_| ParseError::invalid("port", p))
            })
            .collect::<Result<_, _>>()?;
        if ports.len() > 1 && ports.len() != hosts.len() {
            return Err(ParseError::HostPortMismatch {
                hosts: hosts.len(),
                ports: ports.len(),
            });
        }
        let port_for = |i: usize| match ports.len() {
            0 => None,
            1 => ports[0],
            _ => ports[i],
        };
        self.info.params.hosts = if hosts.is_empty() && !ports.is_empty() {
            // Port with no host: libpq uses the default host (localhost / default socket).
            vec![HostSpec {
                host: String::new(),
                port: port_for(0),
            }]
        } else {
            hosts
                .into_iter()
                .enumerate()
                .map(|(i, host)| HostSpec {
                    host,
                    port: port_for(i),
                })
                .collect()
        };
        Ok(self.info)
    }
}

/// Splits `a,b,c`; an empty string yields one empty entry only if a comma is present.
fn split_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split(',').map(str::to_string).collect()
    }
}

fn decode(s: &str) -> Result<String, ParseError> {
    percent_decode_str(s)
        .decode_utf8()
        .map(|c| c.into_owned())
        .map_err(|_| ParseError::Malformed(format!("invalid percent-encoding in {s:?}")))
}

fn parse_uri(input: &str) -> Result<ParsedConnInfo, ParseError> {
    let rest = input
        .strip_prefix("postgresql://")
        .or_else(|| input.strip_prefix("postgres://"))
        .expect("caller checked the scheme");
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };
    let (authority, dbname) = match rest.split_once('/') {
        Some((a, d)) => (a, Some(d)),
        None => (rest, None),
    };
    let mut b = Builder::default();

    let hostlist = match authority.rsplit_once('@') {
        Some((userinfo, hostlist)) => {
            let (user, password) = match userinfo.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (userinfo, None),
            };
            if !user.is_empty() {
                b.set("user", &decode(user)?)?;
            }
            if let Some(p) = password {
                b.set("password", &decode(p)?)?;
            }
            hostlist
        }
        None => authority,
    };

    // host[:port][,host[:port]]... where host may be `[ipv6]` or percent-encoded socket path.
    let (mut hosts, mut ports) = (Vec::new(), Vec::new());
    let mut any_port = false;
    if !hostlist.is_empty() {
        for entry in hostlist.split(',') {
            let (host, port) = if let Some(inner) = entry.strip_prefix('[') {
                let (h, after) = inner
                    .split_once(']')
                    .ok_or_else(|| ParseError::Malformed(format!("unterminated `[` in {entry:?}")))?;
                (h, after.strip_prefix(':'))
            } else {
                match entry.rsplit_once(':') {
                    Some((h, p)) => (h, Some(p)),
                    None => (entry, None),
                }
            };
            hosts.push(decode(host)?);
            match port {
                Some(p) if !p.is_empty() => {
                    any_port = true;
                    ports.push(p.to_string());
                }
                _ => ports.push(String::new()),
            }
        }
    }
    // Per-host ports here may be blank, unlike the list form, so resolve them immediately.
    if !hosts.is_empty() {
        b.hosts = hosts;
        if any_port {
            b.ports = ports;
        }
    }

    if let Some(db) = dbname.filter(|d| !d.is_empty()) {
        b.set("dbname", &decode(db)?)?;
    }
    if let Some(q) = query {
        for pair in q.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| ParseError::Malformed(format!("missing `=` in {pair:?}")))?;
            // A query `host`/`port` overrides the authority (used for Unix sockets).
            b.set(&decode(k)?, &decode(v)?)?;
        }
    }
    // Blank per-host ports (`h1,h2:5433`) mean "default"; drop them before validation.
    if b.ports.iter().any(String::is_empty) {
        let ports = std::mem::take(&mut b.ports);
        return b.finish_with_blank_ports(&ports);
    }
    b.finish()
}

impl Builder {
    fn finish_with_blank_ports(mut self, ports: &[String]) -> Result<ParsedConnInfo, ParseError> {
        let hosts = std::mem::take(&mut self.hosts);
        let mut specs = Vec::with_capacity(hosts.len());
        for (i, host) in hosts.into_iter().enumerate() {
            let port = match ports.get(i).map(String::as_str) {
                None | Some("") => None,
                Some(p) => Some(p.parse::<u16>().map_err(|_| ParseError::invalid("port", p))?),
            };
            specs.push(HostSpec { host, port });
        }
        self.info.params.hosts = specs;
        Ok(self.info)
    }
}

fn parse_keyword_value(input: &str) -> Result<ParsedConnInfo, ParseError> {
    let mut b = Builder::default();
    let mut chars = input.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c.is_whitespace() {
                break;
            }
            key.push(c);
            chars.next();
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next() != Some('=') {
            return Err(ParseError::Malformed(format!("missing `=` after `{key}`")));
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'\'') {
            chars.next();
            loop {
                match chars.next() {
                    Some('\\') => match chars.next() {
                        Some(c) => value.push(c),
                        None => return Err(ParseError::Malformed("trailing backslash".into())),
                    },
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => return Err(ParseError::Malformed("unterminated quoted value".into())),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                if c == '\\' {
                    chars.next();
                    if let Some(n) = chars.next() {
                        value.push(n);
                    }
                    continue;
                }
                value.push(c);
                chars.next();
            }
        }
        if key.is_empty() {
            return Err(ParseError::Malformed("empty keyword".into()));
        }
        b.set(&key, &value)?;
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ConnectionParams {
        parse_conninfo(s).unwrap().params
    }

    fn host(h: &str, p: Option<u16>) -> HostSpec {
        HostSpec {
            host: h.into(),
            port: p,
        }
    }

    #[test]
    fn uri_full() {
        let p = parse("postgresql://bob:s%40cret@db.example.com:6543/shop?sslmode=verify-full&connect_timeout=7&application_name=x%20y");
        assert_eq!(p.user.as_deref(), Some("bob"));
        assert_eq!(p.password.as_deref(), Some("s@cret"));
        assert_eq!(p.hosts, vec![host("db.example.com", Some(6543))]);
        assert_eq!(p.dbname.as_deref(), Some("shop"));
        assert_eq!(p.sslmode, SslMode::VerifyFull);
        assert_eq!(p.connect_timeout, Some(Duration::from_secs(7)));
        assert_eq!(p.application_name.as_deref(), Some("x y"));
    }

    #[test]
    fn uri_minimal_and_multi_host() {
        assert_eq!(parse("postgres://").hosts, vec![]);
        assert_eq!(parse("postgres:///mydb").dbname.as_deref(), Some("mydb"));
        let p = parse("postgresql://h1:5432,h2,h3:5434/db");
        assert_eq!(
            p.hosts,
            vec![host("h1", Some(5432)), host("h2", None), host("h3", Some(5434))]
        );
    }

    #[test]
    fn uri_ipv6_and_unix_socket() {
        let p = parse("postgresql://[2001:db8::1]:5433/db");
        assert_eq!(p.hosts, vec![host("2001:db8::1", Some(5433))]);
        let p = parse("postgresql:///db?host=/var/run/postgresql");
        assert_eq!(p.hosts, vec![host("/var/run/postgresql", None)]);
        assert!(p.hosts[0].is_unix_socket());
        let p = parse("postgresql://%2Fvar%2Frun%2Fpostgresql/db");
        assert!(p.hosts[0].is_unix_socket());
    }

    #[test]
    fn keyword_value() {
        let p = parse(r"host=h1,h2 port=5432,5433 dbname='my db' user = bob password='it\'s' sslmode=require");
        assert_eq!(p.hosts, vec![host("h1", Some(5432)), host("h2", Some(5433))]);
        assert_eq!(p.dbname.as_deref(), Some("my db"));
        assert_eq!(p.password.as_deref(), Some("it's"));
        assert_eq!(p.sslmode, SslMode::Require);
    }

    #[test]
    fn single_port_applies_to_all_hosts() {
        let p = parse("host=a,b port=6000");
        assert_eq!(p.hosts, vec![host("a", Some(6000)), host("b", Some(6000))]);
    }

    #[test]
    fn port_without_host() {
        assert_eq!(parse("port=5544").hosts, vec![host("", Some(5544))]);
    }

    #[test]
    fn mismatched_lists_are_rejected() {
        assert_eq!(
            parse_conninfo("host=a,b,c port=1,2").unwrap_err(),
            ParseError::HostPortMismatch { hosts: 3, ports: 2 }
        );
    }

    #[test]
    fn unsupported_keys_are_reported_not_dropped() {
        let info = parse_conninfo("host=h gssencmode=require krbsrvname=x").unwrap();
        assert_eq!(info.unsupported.len(), 2);
        assert_eq!(info.unsupported["gssencmode"], "require");
    }

    #[test]
    fn errors() {
        assert!(matches!(
            parse_conninfo("hots=a"),
            Err(ParseError::UnknownParameter(k)) if k == "hots"
        ));
        assert!(matches!(
            parse_conninfo("sslmode=sometimes"),
            Err(ParseError::InvalidValue { .. })
        ));
        assert!(matches!(parse_conninfo("host"), Err(ParseError::Malformed(_))));
        assert!(matches!(parse_conninfo("host='abc"), Err(ParseError::Malformed(_))));
        assert!(matches!(
            parse_conninfo("postgresql://[::1/db"),
            Err(ParseError::Malformed(_))
        ));
        assert!(matches!(
            parse_conninfo("port=99999"),
            Err(ParseError::InvalidValue { .. })
        ));
    }

    #[test]
    fn zero_connect_timeout_means_no_timeout() {
        assert_eq!(parse("connect_timeout=0").connect_timeout, None);
    }

    #[test]
    fn password_is_never_serialized() {
        let mut p = ConnectionParams::default();
        p.password = Some("hunter2".into());
        assert!(!serde_json::to_string(&p).unwrap().contains("hunter2"));
    }
}
