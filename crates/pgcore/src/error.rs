//! Turns driver, IO and SQLSTATE failures into messages a user can act on.
//!
//! [`classify`] is a pure function over a plain [`ErrorInput`] so every failure mode can be unit
//! tested without a server; [`UserFacingError::from_pg`] extracts that input from a
//! `tokio_postgres::Error`.

use std::error::Error as StdError;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    DnsFailure,
    ConnectionRefused,
    Timeout,
    Unreachable,
    Tls,
    Authentication,
    Permission,
    DatabaseNotFound,
    ConnectionLost,
    TooManyConnections,
    Cancelled,
    Syntax,
    ObjectNotFound,
    Constraint,
    ReadOnly,
    Serialization,
    /// An edit matched no row: someone else changed or deleted it.
    Conflict,
    Config,
    Unsupported,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserFacingError {
    pub kind: ErrorKind,
    /// One line, suitable for a banner or toast.
    pub title: String,
    /// What went wrong, in terms of the user's situation.
    pub detail: String,
    pub hint: Option<String>,
    pub sqlstate: Option<String>,
    /// 1-based character offset into the statement (`POSITION` in a Postgres error).
    pub position: Option<u32>,
    /// True when repeating the same request may succeed (deadlock, dropped connection, ...).
    pub retryable: bool,
    /// The unprocessed error text, for a "show details" disclosure.
    pub raw: String,
}

impl std::fmt::Display for UserFacingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.title, self.detail)
    }
}

impl StdError for UserFacingError {}

/// Convenience for call sites without a connection target to name; prefer [`UserFacingError::from_pg`]
/// with a target where one is known, because the message is clearer.
impl From<tokio_postgres::Error> for UserFacingError {
    fn from(err: tokio_postgres::Error) -> Self {
        UserFacingError::from_pg(&err, None)
    }
}

impl UserFacingError {
    /// An error produced by our own validation rather than by the server or network.
    pub fn config(title: impl Into<String>, detail: impl Into<String>) -> Self {
        let (title, detail) = (title.into(), detail.into());
        Self {
            kind: ErrorKind::Config,
            raw: format!("{title}: {detail}"),
            title,
            detail,
            hint: None,
            sqlstate: None,
            position: None,
            retryable: false,
        }
    }

    pub fn unsupported(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unsupported,
            ..Self::config(title, detail)
        }
    }

    /// Classifies a driver error. `target` is `host:port` (or a socket path) for context.
    pub fn from_pg(err: &tokio_postgres::Error, target: Option<&str>) -> Self {
        let mut input = ErrorInput {
            target,
            raw: full_chain(err),
            ..Default::default()
        };
        if let Some(db) = err.as_db_error() {
            input.sqlstate = Some(db.code().code());
            input.message = db.message();
            input.db_detail = db.detail();
            input.db_hint = db.hint();
            input.position = db.position().and_then(|p| match p {
                tokio_postgres::error::ErrorPosition::Original(n) => Some(*n),
                tokio_postgres::error::ErrorPosition::Internal { .. } => None,
            });
        } else {
            input.message = "";
            input.closed = err.is_closed();
            let mut source: Option<&(dyn StdError + 'static)> = Some(err);
            while let Some(e) = source {
                if let Some(io_err) = e.downcast_ref::<io::Error>() {
                    input.io_kind = Some(io_err.kind());
                    input.io_message = Some(io_err.to_string());
                    break;
                }
                source = e.source();
            }
        }
        classify(&input)
    }
}

fn full_chain(err: &dyn StdError) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        out.push_str(": ");
        out.push_str(&e.to_string());
        source = e.source();
    }
    out
}

#[derive(Debug, Default)]
pub struct ErrorInput<'a> {
    pub sqlstate: Option<&'a str>,
    pub message: &'a str,
    pub db_detail: Option<&'a str>,
    pub db_hint: Option<&'a str>,
    pub position: Option<u32>,
    pub io_kind: Option<io::ErrorKind>,
    pub io_message: Option<String>,
    /// The driver reported the connection as closed.
    pub closed: bool,
    pub target: Option<&'a str>,
    pub raw: String,
}

pub fn classify(input: &ErrorInput) -> UserFacingError {
    let at = input
        .target
        .map(|t| format!(" at {t}"))
        .unwrap_or_default();
    let server_msg = input.message.to_string();
    let mut e = UserFacingError {
        kind: ErrorKind::Other,
        title: "Database error".into(),
        detail: if server_msg.is_empty() {
            input.raw.clone()
        } else {
            server_msg.clone()
        },
        hint: input.db_hint.map(str::to_string),
        sqlstate: input.sqlstate.map(str::to_string),
        position: input.position,
        retryable: false,
        raw: input.raw.clone(),
    };
    let set = |e: &mut UserFacingError, kind, title: &str, detail: String| {
        e.kind = kind;
        e.title = title.to_string();
        e.detail = detail;
    };

    if let Some(code) = input.sqlstate {
        // Full codes first, then classes (first two characters).
        match code {
            "28P01" => {
                set(&mut e, ErrorKind::Authentication, "Authentication failed",
                    format!("The server{at} rejected the password for this user."));
                e.hint = Some("Check the user name and password; passwords are case-sensitive.".into());
            }
            "28000" => {
                set(&mut e, ErrorKind::Authentication, "Access denied", server_msg.clone());
                if server_msg.contains("pg_hba.conf") {
                    e.hint = Some(
                        "The server's pg_hba.conf has no rule allowing this user, database, address \
                         or SSL setting. Ask the administrator to add one, or try a different SSL mode."
                            .into(),
                    );
                }
            }
            "3D000" => set(&mut e, ErrorKind::DatabaseNotFound, "Database not found", server_msg.clone()),
            "42501" => {
                set(&mut e, ErrorKind::Permission, "Permission denied", server_msg.clone());
                e.hint = Some("Ask a database administrator to grant the missing privilege.".into());
            }
            "57014" => set(&mut e, ErrorKind::Cancelled, "Query cancelled",
                           "The query was cancelled or exceeded the statement timeout.".into()),
            "53300" => {
                set(&mut e, ErrorKind::TooManyConnections, "Too many connections", server_msg.clone());
                e.retryable = true;
            }
            "40001" | "40P01" => {
                set(&mut e, ErrorKind::Serialization,
                    if code == "40P01" { "Deadlock detected" } else { "Serialization failure" },
                    server_msg.clone());
                e.retryable = true;
                e.hint = Some("The transaction was rolled back; running it again usually succeeds.".into());
            }
            "25006" => set(&mut e, ErrorKind::ReadOnly, "Read-only transaction",
                           "This connection or transaction is read-only.".into()),
            "57P01" | "57P02" | "57P03" => {
                set(&mut e, ErrorKind::ConnectionLost, "Connection closed by server", server_msg.clone());
                e.retryable = true;
            }
            "42P01" | "42703" | "42883" | "3F000" => {
                set(&mut e, ErrorKind::ObjectNotFound, "Object not found", server_msg.clone())
            }
            c if c.starts_with("08") => {
                set(&mut e, ErrorKind::ConnectionLost, "Connection problem", server_msg.clone());
                e.retryable = true;
            }
            c if c.starts_with("28") => set(&mut e, ErrorKind::Authentication, "Authentication failed", server_msg.clone()),
            c if c.starts_with("23") => {
                let title = match c {
                    "23505" => "Duplicate value",
                    "23503" => "Foreign key violation",
                    "23502" => "Missing required value",
                    "23514" => "Check constraint violated",
                    _ => "Constraint violation",
                };
                set(&mut e, ErrorKind::Constraint, title, server_msg.clone());
                if let Some(d) = input.db_detail {
                    e.detail = format!("{server_msg}\n{d}");
                }
            }
            "54000" => {
                // program_limit_exceeded: index entry too big, jsonb element limit, row too wide...
                set(&mut e, ErrorKind::Other, "Value exceeds a database limit", server_msg.clone());
                let lower = server_msg.to_lowercase();
                e.hint = Some(if lower.contains("index row") {
                    "Postgres cannot index a value this large (a btree entry is limited to a few KB). \
                     Edit a column that has no such index, or change the index (for example to a hash of the value)."
                } else if lower.contains("jsonb") {
                    "jsonb objects and arrays are limited to about 256 MB of elements. Store the document as json or text, or split it up."
                } else {
                    "The value is larger than this Postgres server allows here."
                }
                .into());
            }
            "42601" => set(&mut e, ErrorKind::Syntax, "SQL syntax error", server_msg.clone()),
            c if c.starts_with("42") => set(&mut e, ErrorKind::Syntax, "SQL error", server_msg.clone()),
            c if c.starts_with("57") => set(&mut e, ErrorKind::ConnectionLost, "Server is shutting down or unavailable", server_msg.clone()),
            _ => {}
        }
        return e;
    }

    // Not a server error: network, TLS, or driver-level.
    let raw_lower = input.raw.to_lowercase();
    let io_msg = input.io_message.as_deref().unwrap_or("").to_lowercase();
    let is_dns = io_msg.contains("lookup address")
        || io_msg.contains("name or service not known")
        || io_msg.contains("nodename nor servname")
        || io_msg.contains("no address associated");

    if raw_lower.contains("tls") || raw_lower.contains("ssl") || raw_lower.contains("certificate") {
        set(&mut e, ErrorKind::Tls, "Secure connection failed",
            format!("Could not establish a TLS connection{at}."));
        e.hint = Some(
            "If the server does not offer SSL, choose a different SSL mode; if it uses a private CA, \
             point the connection at that CA certificate."
                .into(),
        );
    } else if is_dns {
        set(&mut e, ErrorKind::DnsFailure, "Host not found",
            format!("The address{} could not be resolved.", input.target.map(|t| format!(" {t}")).unwrap_or_default()));
        e.hint = Some("Check the spelling of the host name and your network or VPN connection.".into());
    } else if let Some(kind) = input.io_kind {
        use io::ErrorKind as K;
        match kind {
            K::ConnectionRefused => {
                set(&mut e, ErrorKind::ConnectionRefused, "Connection refused",
                    format!("Nothing is accepting connections{at}."));
                e.hint = Some("Check that the server is running and that the host and port are correct.".into());
                e.retryable = true;
            }
            K::TimedOut => {
                set(&mut e, ErrorKind::Timeout, "Connection timed out",
                    format!("The server{at} did not respond in time."));
                e.hint = Some("A firewall may be dropping packets, or the host is unreachable from this network.".into());
                e.retryable = true;
            }
            K::HostUnreachable | K::NetworkUnreachable | K::NetworkDown => {
                set(&mut e, ErrorKind::Unreachable, "Host unreachable",
                    format!("There is no route to the server{at}."));
                e.hint = Some("Check your network or VPN connection.".into());
                e.retryable = true;
            }
            K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::UnexpectedEof => {
                set(&mut e, ErrorKind::ConnectionLost, "Connection lost",
                    format!("The connection to the server{at} was interrupted."));
                e.retryable = true;
            }
            K::NotFound | K::PermissionDenied if input.target.is_some_and(|t| t.starts_with('/')) => {
                set(&mut e, ErrorKind::ConnectionRefused, "Cannot open socket",
                    format!("Could not use the Unix socket{at}."));
                e.hint = Some("Check the socket directory and that your user may access it.".into());
            }
            _ => {
                set(&mut e, ErrorKind::Other, "Network error", input.io_message.clone().unwrap_or_else(|| input.raw.clone()));
            }
        }
    } else if input.closed {
        set(&mut e, ErrorKind::ConnectionLost, "Connection lost",
            "The connection to the server is closed.".into());
        e.retryable = true;
    } else if raw_lower.contains("timeout") || raw_lower.contains("timed out") {
        set(&mut e, ErrorKind::Timeout, "Connection timed out",
            format!("The server{at} did not respond in time."));
        e.retryable = true;
    } else if raw_lower.contains("invalid configuration") || raw_lower.contains("invalid") && raw_lower.contains("config") {
        set(&mut e, ErrorKind::Config, "Invalid connection settings", input.raw.clone());
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(code: &str, message: &str) -> UserFacingError {
        classify(&ErrorInput {
            sqlstate: Some(code),
            message,
            raw: message.into(),
            ..Default::default()
        })
    }

    fn net(kind: io::ErrorKind, msg: &str, target: &str) -> UserFacingError {
        classify(&ErrorInput {
            io_kind: Some(kind),
            io_message: Some(msg.into()),
            target: Some(target),
            raw: format!("error connecting to server: {msg}"),
            ..Default::default()
        })
    }

    #[test]
    fn wrong_password() {
        let e = db("28P01", "password authentication failed for user \"bob\"");
        assert_eq!(e.kind, ErrorKind::Authentication);
        assert!(e.hint.is_some());
        assert!(!e.retryable);
    }

    #[test]
    fn pg_hba_rejection_gets_a_specific_hint() {
        let e = db("28000", "no pg_hba.conf entry for host \"10.0.0.5\", user \"bob\", database \"x\", no encryption");
        assert_eq!(e.kind, ErrorKind::Authentication);
        assert!(e.hint.unwrap().contains("pg_hba.conf"));
    }

    #[test]
    fn permission_denied() {
        let e = db("42501", "permission denied for table secrets");
        assert_eq!(e.kind, ErrorKind::Permission);
        assert_eq!(e.detail, "permission denied for table secrets");
    }

    #[test]
    fn missing_database() {
        assert_eq!(db("3D000", "database \"nope\" does not exist").kind, ErrorKind::DatabaseNotFound);
    }

    #[test]
    fn cancellation_and_deadlock() {
        assert_eq!(db("57014", "canceling statement due to user request").kind, ErrorKind::Cancelled);
        let dl = db("40P01", "deadlock detected");
        assert_eq!(dl.kind, ErrorKind::Serialization);
        assert!(dl.retryable);
    }

    #[test]
    fn admin_shutdown_and_connection_class_are_retryable_connection_loss() {
        for code in ["57P01", "57P02", "08006", "08003"] {
            let e = db(code, "x");
            assert_eq!(e.kind, ErrorKind::ConnectionLost, "{code}");
            assert!(e.retryable, "{code}");
        }
    }

    #[test]
    fn constraint_violations_carry_server_detail() {
        let e = classify(&ErrorInput {
            sqlstate: Some("23505"),
            message: "duplicate key value violates unique constraint \"users_email_key\"",
            db_detail: Some("Key (email)=(a@b.c) already exists."),
            raw: String::new(),
            ..Default::default()
        });
        assert_eq!(e.kind, ErrorKind::Constraint);
        assert_eq!(e.title, "Duplicate value");
        assert!(e.detail.contains("Key (email)=(a@b.c) already exists."));
    }

    #[test]
    fn syntax_errors_keep_their_position() {
        let e = classify(&ErrorInput {
            sqlstate: Some("42601"),
            message: "syntax error at or near \"FORM\"",
            position: Some(10),
            raw: String::new(),
            ..Default::default()
        });
        assert_eq!(e.kind, ErrorKind::Syntax);
        assert_eq!(e.position, Some(10));
    }

    #[test]
    fn network_failures_name_the_target() {
        let e = net(io::ErrorKind::ConnectionRefused, "Connection refused", "localhost:5432");
        assert_eq!(e.kind, ErrorKind::ConnectionRefused);
        assert!(e.detail.contains("localhost:5432"));
        assert!(e.retryable);

        assert_eq!(net(io::ErrorKind::TimedOut, "timed out", "db:5432").kind, ErrorKind::Timeout);
        assert_eq!(net(io::ErrorKind::HostUnreachable, "no route", "db:5432").kind, ErrorKind::Unreachable);
        assert_eq!(net(io::ErrorKind::ConnectionReset, "reset", "db:5432").kind, ErrorKind::ConnectionLost);
    }

    #[test]
    fn dns_failures_are_recognised_by_message() {
        let e = net(io::ErrorKind::Other, "failed to lookup address information: nodename nor servname provided, or not known", "nope.invalid:5432");
        assert_eq!(e.kind, ErrorKind::DnsFailure);
        assert!(e.hint.is_some());
    }

    #[test]
    fn tls_failures_are_not_reported_as_generic_network_errors() {
        let e = classify(&ErrorInput {
            raw: "error performing TLS handshake: invalid peer certificate: UnknownIssuer".into(),
            target: Some("db:5432"),
            ..Default::default()
        });
        assert_eq!(e.kind, ErrorKind::Tls);
        assert!(e.hint.unwrap().contains("CA"));
    }

    #[test]
    fn closed_connection_without_io_error() {
        let e = classify(&ErrorInput { closed: true, raw: "connection closed".into(), ..Default::default() });
        assert_eq!(e.kind, ErrorKind::ConnectionLost);
        assert!(e.retryable);
    }

    #[test]
    fn program_limit_errors_explain_index_and_jsonb_limits() {
        let index = db("54000", "index row requires 24760 bytes, maximum size is 8191");
        assert_eq!(index.title, "Value exceeds a database limit");
        assert!(index.hint.unwrap().contains("index"));
        let jsonb = db("54000", "total size of jsonb object elements exceeds the maximum of 268435455 bytes");
        assert!(jsonb.hint.unwrap().contains("256 MB"));
        assert!(db("54000", "something else").hint.is_some());
    }

    #[test]
    fn unknown_sqlstate_still_surfaces_the_server_message() {
        let e = db("XX000", "internal error text");
        assert_eq!(e.kind, ErrorKind::Other);
        assert_eq!(e.detail, "internal error text");
        assert_eq!(e.sqlstate.as_deref(), Some("XX000"));
    }
}
