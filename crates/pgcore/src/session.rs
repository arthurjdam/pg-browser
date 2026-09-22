//! One live connection to a server: config mapping, connect, state tracking, cancel.

use crate::config::{ChannelBinding, ConnectionParams, SslMode, TargetSessionAttrs};
use crate::error::UserFacingError;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_postgres::{CancelToken, Client, Config, NoTls};

const DEFAULT_PORT: u16 = 5432;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_APPLICATION_NAME: &str = "pg-browser";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub version: String,
    pub version_num: i32,
    pub database: String,
    pub user: String,
    /// `host:port` (or socket path) that actually answered.
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Connected,
    /// The connection task ended; carries the reason when the driver reported one.
    Closed(Option<UserFacingError>),
}

/// Human-readable `host:port` list, for error messages.
pub fn describe_target(params: &ConnectionParams) -> String {
    if params.hosts.is_empty() {
        return format!("localhost:{DEFAULT_PORT}");
    }
    params
        .hosts
        .iter()
        .map(|h| {
            let host = if h.host.is_empty() { "localhost" } else { &h.host };
            if h.is_unix_socket() {
                host.to_string()
            } else {
                format!("{host}:{}", h.port.unwrap_or(DEFAULT_PORT))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Maps our parameters onto `tokio_postgres::Config`. Pure so it can be tested without a server.
///
/// Refuses (rather than silently weakening) anything this build cannot honour: a `require`/
/// `verify-*` SSL mode must never end up as a plaintext connection.
pub fn build_config(params: &ConnectionParams) -> Result<Config, UserFacingError> {
    match params.sslmode {
        SslMode::Disable | SslMode::Allow | SslMode::Prefer => {}
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
            return Err(UserFacingError::unsupported(
                "TLS is not available yet",
                "This connection requires an encrypted connection, but TLS support is not built in \
                 yet. Use SSL mode `prefer` or `disable` for a server that allows plaintext.",
            ));
        }
    }
    let mut cfg = Config::new();
    cfg.ssl_mode(tokio_postgres::config::SslMode::Disable);

    if params.hosts.is_empty() {
        cfg.host("localhost").port(DEFAULT_PORT);
    }
    for h in &params.hosts {
        let host = if h.host.is_empty() { "localhost" } else { h.host.as_str() };
        if h.is_unix_socket() {
            cfg.host_path(host);
        } else {
            cfg.host(host);
        }
        cfg.port(h.port.unwrap_or(DEFAULT_PORT));
    }

    let user = params
        .user
        .clone()
        .or_else(|| std::env::var("PGUSER").ok())
        .or_else(|| std::env::var("USER").ok())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| UserFacingError::config("No user name", "Enter a user name for this connection."))?;
    cfg.user(&user);
    if let Some(pw) = &params.password {
        cfg.password(pw);
    }
    cfg.dbname(params.dbname.as_deref().unwrap_or(&user));
    cfg.application_name(params.application_name.as_deref().unwrap_or(DEFAULT_APPLICATION_NAME));
    if let Some(o) = &params.options {
        cfg.options(o);
    }
    cfg.connect_timeout(params.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT));
    if let Some(v) = params.keepalives {
        cfg.keepalives(v);
    }
    if let Some(v) = params.keepalives_idle {
        cfg.keepalives_idle(v);
    }
    if let Some(v) = params.keepalives_interval {
        cfg.keepalives_interval(v);
    }
    if let Some(v) = params.keepalives_retries {
        cfg.keepalives_retries(v);
    }
    if let Some(v) = params.tcp_user_timeout {
        cfg.tcp_user_timeout(v);
    }
    cfg.channel_binding(match params.channel_binding {
        ChannelBinding::Disable => tokio_postgres::config::ChannelBinding::Disable,
        ChannelBinding::Prefer => tokio_postgres::config::ChannelBinding::Prefer,
        ChannelBinding::Require => tokio_postgres::config::ChannelBinding::Require,
    });
    match params.target_session_attrs {
        TargetSessionAttrs::Any => {}
        TargetSessionAttrs::ReadWrite => {
            cfg.target_session_attrs(tokio_postgres::config::TargetSessionAttrs::ReadWrite);
        }
        other => {
            return Err(UserFacingError::unsupported(
                "Unsupported target_session_attrs",
                format!("`{other:?}` is not supported yet; use `any` or `read-write`."),
            ));
        }
    }
    if params.load_balance_random {
        cfg.load_balance_hosts(tokio_postgres::config::LoadBalanceHosts::Random);
    }
    Ok(cfg)
}

/// A connected session. Cheap to clone (`Arc` inside); dropping the last clone closes it.
#[derive(Clone)]
pub struct Session {
    client: Arc<Client>,
    cancel: CancelToken,
    state: watch::Receiver<SessionState>,
    info: Arc<ServerInfo>,
}

impl Session {
    /// Must be called from within a tokio runtime (it spawns the connection driver task).
    pub async fn connect(params: &ConnectionParams) -> Result<Session, UserFacingError> {
        let cfg = build_config(params)?;
        let target = describe_target(params);
        let (client, connection) = cfg
            .connect(NoTls)
            .await
            .map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
        let cancel = client.cancel_token();

        let (tx, state) = watch::channel(SessionState::Connected);
        let driver_target = target.clone();
        tokio::spawn(async move {
            let result = connection.await;
            let reason = result.err().map(|e| UserFacingError::from_pg(&e, Some(&driver_target)));
            let _ = tx.send(SessionState::Closed(reason));
        });

        let client = Arc::new(client);
        let row = client
            .query_one(
                "SELECT version(), current_setting('server_version_num')::int, \
                 current_database()::text, current_user::text",
                &[],
            )
            .await
            .map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
        let info = Arc::new(ServerInfo {
            version: row.get(0),
            version_num: row.get(1),
            database: row.get(2),
            user: row.get(3),
            endpoint: target,
        });
        Ok(Session { client, cancel, state, info })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    pub fn state(&self) -> watch::Receiver<SessionState> {
        self.state.clone()
    }

    pub fn is_connected(&self) -> bool {
        matches!(*self.state.borrow(), SessionState::Connected)
    }

    /// Asks the server to cancel whatever this session is running. Uses a separate connection,
    /// as the protocol requires, so it works while a query is blocking the main one.
    pub async fn cancel(&self) -> Result<(), UserFacingError> {
        self.cancel
            .cancel_query(NoTls)
            .await
            .map_err(|e| UserFacingError::from_pg(&e, Some(&self.info.endpoint)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HostSpec, parse_conninfo};
    use crate::error::ErrorKind;

    fn params(s: &str) -> ConnectionParams {
        parse_conninfo(s).unwrap().params
    }

    #[test]
    fn maps_hosts_ports_and_defaults() {
        let cfg = build_config(&params("postgresql://bob:pw@a:6000,b/shop")).unwrap();
        assert_eq!(cfg.get_user(), Some("bob"));
        assert_eq!(cfg.get_dbname(), Some("shop"));
        assert_eq!(cfg.get_hosts().len(), 2);
        assert_eq!(cfg.get_ports(), &[6000, 5432]);
        assert_eq!(cfg.get_application_name(), Some("pg-browser"));
        assert_eq!(cfg.get_connect_timeout(), Some(&Duration::from_secs(10)));
        assert_eq!(cfg.get_password(), Some(&b"pw"[..]));
    }

    #[test]
    fn empty_host_list_means_localhost() {
        let cfg = build_config(&params("user=bob")).unwrap();
        assert_eq!(cfg.get_hosts().len(), 1);
        assert_eq!(cfg.get_ports(), &[5432]);
        assert_eq!(describe_target(&params("user=bob")), "localhost:5432");
    }

    #[test]
    fn unix_socket_hosts_are_paths() {
        let cfg = build_config(&params("host=/var/run/postgresql user=bob")).unwrap();
        assert!(matches!(cfg.get_hosts()[0], tokio_postgres::config::Host::Unix(_)));
        assert_eq!(describe_target(&params("host=/var/run/postgresql")), "/var/run/postgresql");
    }

    #[test]
    fn dbname_defaults_to_user_like_libpq() {
        let cfg = build_config(&params("host=h user=carol")).unwrap();
        assert_eq!(cfg.get_dbname(), Some("carol"));
    }

    #[test]
    fn explicit_timeouts_and_application_name_are_kept() {
        let cfg = build_config(&params("host=h user=u connect_timeout=3 application_name=mine")).unwrap();
        assert_eq!(cfg.get_connect_timeout(), Some(&Duration::from_secs(3)));
        assert_eq!(cfg.get_application_name(), Some("mine"));
    }

    #[test]
    fn encrypted_modes_are_refused_never_downgraded() {
        for mode in ["require", "verify-ca", "verify-full"] {
            let err = build_config(&params(&format!("host=h user=u sslmode={mode}"))).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Unsupported, "{mode}");
        }
        for mode in ["disable", "allow", "prefer"] {
            assert!(build_config(&params(&format!("host=h user=u sslmode={mode}"))).is_ok(), "{mode}");
        }
    }

    #[test]
    fn unimplemented_target_session_attrs_are_refused() {
        let err = build_config(&params("host=h user=u target_session_attrs=standby")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unsupported);
        assert!(build_config(&params("host=h user=u target_session_attrs=read-write")).is_ok());
    }

    #[test]
    fn describe_target_lists_every_host() {
        let mut p = ConnectionParams::default();
        p.hosts = vec![
            HostSpec { host: "a".into(), port: Some(1) },
            HostSpec { host: "b".into(), port: None },
        ];
        assert_eq!(describe_target(&p), "a:1, b:5432");
    }
}
