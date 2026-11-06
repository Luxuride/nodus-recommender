//! The instance header and the host validation it needs.

/// Header naming the instance the token belongs to.
pub const INSTANCE_HEADER: &str = "x-instance-host";

/// Reject anything that is not a bare host name.
///
/// The host is interpolated into a URL, so allowing a scheme, a path, a userinfo
/// section or whitespace would let a caller point our verified request at a host of
/// their choosing.
pub(super) fn is_plausible_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.contains('/')
        && !host.contains('@')
        && !host.contains(' ')
        && !host.contains("://")
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}
