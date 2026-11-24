use super::HostComments;
use crate::domain::host::{Host, HostField};

pub(crate) fn render_host(host: &Host, comments: Option<&HostComments>) -> String {
    let mut lines = Vec::new();
    if let Some(comments) = comments {
        lines.extend(comments.leading.iter().cloned());
    }
    lines.push(format!("Host {}", quote(host.alias())));
    let mut directive = |field: HostField, line: String| {
        if let Some(comments) = comments.and_then(|comments| comments.fields.get(&field)) {
            lines.extend(comments.iter().map(|comment| format!("    {comment}")));
        }
        lines.push(format!("    {line}"));
    };
    directive(
        HostField::Hostname,
        format!("HostName {}", quote(host.hostname())),
    );
    if let Some(user) = host.user() {
        directive(HostField::User, format!("User {}", quote(user)));
    }
    if let Some(port) = host.port() {
        directive(HostField::Port, format!("Port {port}"));
    }
    if let Some(identity_file) = host.identity_file() {
        directive(
            HostField::IdentityFile,
            format!("IdentityFile {}", quote(identity_file)),
        );
    }
    if let Some(proxy_jump) = host.proxy_jump() {
        directive(HostField::ProxyJump, format!("ProxyJump {proxy_jump}"));
    }
    if let Some(comments) = comments {
        lines.extend(
            comments
                .trailing
                .iter()
                .map(|comment| format!("    {comment}")),
        );
    }
    lines.join("\n")
}

pub(super) fn quote(value: &str) -> String {
    if value
        .chars()
        .any(|c| c.is_whitespace() || "\\\"'#".contains(c))
    {
        quote_path(value)
    } else {
        value.into()
    }
}

pub(crate) fn quote_path(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
