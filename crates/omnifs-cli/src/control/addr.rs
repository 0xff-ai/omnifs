//! Daemon control-address resolution shared by CLI control clients.

/// Resolve the daemon control address (`host:port`): `OMNIFS_DAEMON_ADDR`
/// when set, else the loopback port the container publishes on the host.
pub(crate) fn daemon_addr() -> String {
    crate::config::resolve_setting(
        None,
        "OMNIFS_DAEMON_ADDR",
        || None,
        omnifs_api::default_listen_addr().to_string(),
    )
}
