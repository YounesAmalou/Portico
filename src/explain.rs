//! Turns a diagnosed failure into words: what went wrong, the facts behind it and the next step.

use std::path::Path;

use crate::{
    config_error::ConfigRejection,
    probe::{CONNECT_TIMEOUT_SECS, Diagnosis, Failure, KeySource, OfferedKey, TEST_DEADLINE},
};

/// The widest a fact or next step runs before it wraps, not counting its label.
const WRAP_COLUMNS: usize = 72;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Test,
    Connect,
    Save,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Label {
    User,
    Address,
    Offered,
    Key,
    Name,
    Methods,
    File,
    Jump,
    Line,
    Next,
}

impl Label {
    fn name(self) -> &'static str {
        match self {
            Label::User => "user",
            Label::Address => "address",
            Label::Offered => "offered",
            Label::Key => "key",
            Label::Name => "name",
            Label::Methods => "methods",
            Label::File => "file",
            Label::Jump => "jump",
            Label::Line => "line",
            Label::Next => "next",
        }
    }
}

type Facts = Vec<(Label, Vec<String>)>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Explanation {
    pub headline: String,
    pub facts: Facts,
    pub next: Vec<String>,
    pub raw: Vec<String>,
}

pub struct Context<'a> {
    pub alias: &'a str,
    pub operation: Operation,
    pub user: Option<&'a str>,
    pub hostname: Option<&'a str>,
    pub port: Option<u16>,
    /// Each configured identity file and whether it exists.
    pub identity_files: &'a [(String, bool)],
    /// Whether the jump host is an alias Portico knows, so `portico test <jump>` works.
    pub jump_is_known: bool,
    pub home: &'a Path,
}

struct Parts {
    headline: String,
    facts: Facts,
    next: Vec<String>,
}

impl Parts {
    fn new(headline: &str) -> Self {
        Self {
            headline: headline.into(),
            facts: Vec::new(),
            next: Vec::new(),
        }
    }

    fn fact(mut self, label: Label, value: impl Into<String>) -> Self {
        self.facts.push((label, vec![value.into()]));
        self
    }

    fn facts(mut self, label: Label, values: Vec<String>) -> Self {
        if !values.is_empty() {
            self.facts.push((label, values));
        }
        self
    }

    fn next(mut self, step: impl Into<String>) -> Self {
        self.next.push(step.into());
        self
    }
}

pub fn explain(diagnosis: &Diagnosis, context: &Context<'_>) -> Explanation {
    let parts = describe(&diagnosis.failure, context);
    Explanation {
        headline: parts.headline,
        facts: parts.facts,
        next: parts.next,
        raw: diagnosis.raw.clone(),
    }
}

fn describe(failure: &Failure, context: &Context<'_>) -> Parts {
    let alias = context.alias;
    let testing = context.operation != Operation::Connect;
    match failure {
        Failure::AuthRefused {
            methods,
            offered,
            unusable,
        } => refusal(methods, offered, unusable, context),
        Failure::NameUnresolved { host } => {
            let note = if context.hostname == Some(alias) {
                " (no HostName is set, so ssh looked up the alias)"
            } else {
                ""
            };
            Parts::new("the host name does not resolve")
                .fact(Label::Name, format!("{host}{note}"))
                .next(format!("check HostName for {alias}"))
        }
        Failure::ConnectionRefused => {
            with_address(Parts::new("nothing accepted the connection"), context)
                .next("check the port, and that an SSH server runs there")
        }
        Failure::Unreachable => with_address(Parts::new("the host is unreachable"), context)
            .next("check the address and your network"),
        Failure::TimedOut { deadline } => {
            let line = if *deadline {
                format!("the test gave up after {} s", TEST_DEADLINE.as_secs())
            } else {
                format!("no answer within {CONNECT_TIMEOUT_SECS} s")
            };
            with_address(Parts::new("the host did not answer in time"), context)
                .fact(Label::Line, line)
                .next("check the address, the port and any firewall")
        }
        Failure::JumpHostFailed { jump, cause } => {
            let inner = Context {
                alias: jump,
                user: None,
                hostname: None,
                port: None,
                ..*context
            };
            let cause = describe(cause, &inner);
            let mut flat = Vec::new();
            for (label, values) in &cause.facts {
                for (index, value) in values.iter().enumerate() {
                    flat.push(if index == 0 {
                        format!("{}: {value}", label.name())
                    } else {
                        value.clone()
                    });
                }
            }
            let mut parts = Parts::new(&format!("jump host {jump} failed: {}", cause.headline))
                .facts(Label::Jump, flat);
            if context.jump_is_known {
                parts = parts.next(format!(
                    "run `portico test {jump}` to check the jump host on its own"
                ));
            }
            parts.next.extend(cause.next);
            parts
        }
        Failure::HostKeyUnknown => {
            let parts = Parts::new("the host key is not trusted");
            if testing {
                parts
            } else {
                parts.next(format!(
                    "the key was declined; review it with `portico test {alias}`"
                ))
            }
        }
        Failure::HostKeyChanged { known_hosts } => {
            let mut parts = Parts::new("the host key changed, which can mean an attack");
            if let Some((file, line)) = known_hosts {
                parts = parts.fact(
                    Label::File,
                    format!("{}:{line}", shorten(file, context.home)),
                );
            }
            let host = context.hostname.unwrap_or(alias);
            parts.next(format!(
                "once the new key is confirmed with the server's administrator, remove the old one with `ssh-keygen -R {host}`"
            ))
        }
        Failure::ConfigRejected(rejection) => rejected(rejection, context.home),
        Failure::ConnectionLost { line } => {
            Parts::new("the connection was lost").fact(Label::Line, line.as_str())
        }
        Failure::Other(line) => Parts::new(line),
    }
}

fn refusal(
    methods: &str,
    offered: &[OfferedKey],
    unusable: &[String],
    context: &Context<'_>,
) -> Parts {
    let alias = context.alias;
    let home = context.home;
    if !unusable.is_empty() {
        let mut parts = Parts::new("the key needs a passphrase").facts(
            Label::Key,
            unusable.iter().map(|path| shorten(path, home)).collect(),
        );
        for path in unusable {
            parts = parts.next(format!("load it with `ssh-add {}`", shorten(path, home)));
        }
        return parts;
    }
    if !methods
        .split(',')
        .any(|method| method.trim() == "publickey")
    {
        let parts = Parts::new("the server accepts only a password")
            .fact(Label::Methods, methods.replace(',', ", "));
        return match context.operation {
            Operation::Connect => parts,
            _ => parts.next(format!(
                "the test cannot type a password; `portico connect {alias}` will ask for it"
            )),
        };
    }
    let connecting = context.operation == Operation::Connect;
    if offered.is_empty() && !connecting {
        let missing = context
            .identity_files
            .iter()
            .filter(|(_, exists)| !exists)
            .map(|(path, _)| format!("{} (does not exist)", shorten(path, home)))
            .collect();
        return Parts::new("no key was offered")
            .facts(Label::Key, missing)
            .next(format!(
                "set IdentityFile for {alias}, or load a key in an agent"
            ));
    }
    let mut parts = Parts::new("the server refused the login");
    if let Some((label, value)) = who(context) {
        parts = parts.fact(label, value);
    }
    parts = if offered.is_empty() {
        parts.fact(Label::Offered, "not reported at this log level")
    } else {
        parts.facts(
            Label::Offered,
            offered
                .iter()
                .map(|key| {
                    let source = match key.source {
                        KeySource::File => "file",
                        KeySource::Agent => "agent",
                    };
                    format!("{} ({source})", shorten(&key.path, home))
                })
                .collect(),
        )
    };
    parts = parts.next(format!(
        "check User and IdentityFile for {alias}, and that the public key is in the server's authorized_keys"
    ));
    if offered.is_empty() {
        parts = parts.next(format!(
            "run `portico test {alias}` to see which keys were offered"
        ));
    }
    parts
}

fn rejected(rejection: &ConfigRejection, home: &Path) -> Parts {
    let parts = Parts::new("OpenSSH rejected the configuration");
    let parts = match rejection {
        ConfigRejection::Managed {
            alias,
            directive,
            message,
            ..
        } => parts
            .fact(Label::File, format!("written by Portico, for {alias}"))
            .fact(Label::Line, format!("{directive} ({message})")),
        ConfigRejection::UserFile {
            path,
            line,
            message,
        } => parts
            .fact(Label::File, shorten(&path.display().to_string(), home))
            .fact(Label::Line, format!("{line} ({message})")),
        ConfigRejection::Internal { raw } | ConfigRejection::Unrecognised { raw } => {
            parts.facts(Label::Line, raw.lines().map(str::to_string).collect())
        }
    };
    parts.next("fix that line, then try again")
}

/// `user@host:port`, falling back to what is known.
fn who(context: &Context<'_>) -> Option<(Label, String)> {
    let address = address(context);
    match (context.user, address) {
        (Some(user), Some(address)) => Some((Label::User, format!("{user}@{address}"))),
        (Some(user), None) => Some((Label::User, user.to_string())),
        (None, Some(address)) => Some((Label::Address, address)),
        (None, None) => None,
    }
}

fn address(context: &Context<'_>) -> Option<String> {
    context
        .hostname
        .map(|host| format!("{host}:{}", context.port.unwrap_or(22)))
}

fn with_address(parts: Parts, context: &Context<'_>) -> Parts {
    match address(context) {
        Some(address) => parts.fact(Label::Address, address),
        None => parts,
    }
}

/// Shows a path under `home` with `~`.
fn shorten(path: &str, home: &Path) -> String {
    match Path::new(path).strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => {
            if rest.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", rest.display())
            }
        }
        _ => path.to_string(),
    }
}

/// Breaks `text` into lines of at most `width` columns at spaces; a longer word keeps its own line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split(' ') {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
}

impl Explanation {
    /// `<alias>: failed — <headline>` then the aligned labelled lines (label column padded to
    /// the longest label plus one space, continuation lines indented to the value column), then
    /// the raw lines under `  ssh said:` when `raw` is true, or whenever ssh's own last line is
    /// all the headline there is.
    pub fn render(&self, alias: &str, raw: bool) -> Vec<String> {
        let mut lines = vec![format!("{alias}: failed \u{2014} {}", self.headline)];
        let mut rows: Vec<(Label, &[String])> = self
            .facts
            .iter()
            .map(|(label, values)| (*label, values.as_slice()))
            .collect();
        if !self.next.is_empty() {
            rows.push((Label::Next, &self.next));
        }
        let column = rows
            .iter()
            .map(|(label, _)| label.name().len())
            .max()
            .unwrap_or(0)
            + 2;
        for (label, values) in rows {
            let mut tag = Some(format!("{}:", label.name()));
            for value in values {
                for text in wrap(value, WRAP_COLUMNS) {
                    let prefix = tag.take().unwrap_or_default();
                    lines.push(format!("  {prefix:<column$}{text}"));
                }
            }
        }
        let unrecognised = self.raw.last() == Some(&self.headline);
        if (raw || unrecognised) && !self.raw.is_empty() {
            lines.push("  ssh said:".into());
            lines.extend(self.raw.iter().map(|line| format!("    {line}")));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config_error::ConfigRejection,
        probe::{Route, diagnose},
    };
    use std::path::PathBuf;

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ssh")
            .join(format!("{name}.txt"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn home() -> PathBuf {
        PathBuf::from("/home/me")
    }

    fn context<'a>(
        home: &'a Path,
        operation: Operation,
        identity_files: &'a [(String, bool)],
    ) -> Context<'a> {
        Context {
            alias: "dev",
            operation,
            user: Some("user"),
            hostname: Some("127.0.0.1"),
            port: Some(22999),
            identity_files,
            jump_is_known: false,
            home,
        }
    }

    fn render(name: &str, operation: Operation) -> Vec<String> {
        let home = home();
        let diagnosis = diagnose(&fixture(name), false, Route::Direct);
        explain(&diagnosis, &context(&home, operation, &[])).render("dev", false)
    }

    fn by_hand(failure: Failure, operation: Operation) -> Vec<String> {
        let home = home();
        let diagnosis = Diagnosis {
            failure,
            raw: Vec::new(),
        };
        explain(&diagnosis, &context(&home, operation, &[])).render("dev", false)
    }

    #[test]
    fn a_refused_key_lists_what_was_offered() {
        assert_eq!(
            render("auth-refused", Operation::Test),
            [
                "dev: failed \u{2014} the server refused the login",
                "  user:    user@127.0.0.1:22999",
                "  offered: /tmp/portico-fixture/wrong (file)",
                "  next:    check User and IdentityFile for dev, and that the public key is in the",
                "           server's authorized_keys",
            ]
        );
    }

    #[test]
    fn an_unusable_key_needs_a_passphrase() {
        assert_eq!(
            render("passphrase-key", Operation::Test),
            [
                "dev: failed \u{2014} the key needs a passphrase",
                "  key:  /tmp/portico-fixture/pass",
                "  next: load it with `ssh-add /tmp/portico-fixture/pass`",
            ]
        );
    }

    #[test]
    fn paths_under_home_are_shown_with_a_tilde() {
        let failure = Failure::AuthRefused {
            methods: "publickey".into(),
            offered: vec![OfferedKey {
                path: "/home/me/.ssh/id".into(),
                fingerprint: "SHA256:x".into(),
                source: KeySource::Agent,
            }],
            unusable: vec!["/home/me/.ssh/work".into()],
        };
        assert_eq!(
            by_hand(failure, Operation::Test),
            [
                "dev: failed \u{2014} the key needs a passphrase",
                "  key:  ~/.ssh/work",
                "  next: load it with `ssh-add ~/.ssh/work`",
            ]
        );
    }

    #[test]
    fn no_key_offered_names_the_identity_files_that_are_missing() {
        let home = home();
        let files = [
            ("~/.ssh/gone".to_string(), false),
            ("~/.ssh/here".to_string(), true),
        ];
        let diagnosis = diagnose(&fixture("missing-identity"), false, Route::Direct);
        assert_eq!(
            explain(&diagnosis, &context(&home, Operation::Test, &files)).render("dev", false),
            [
                "dev: failed \u{2014} no key was offered",
                "  key:  ~/.ssh/gone (does not exist)",
                "  next: set IdentityFile for dev, or load a key in an agent",
            ]
        );
    }

    #[test]
    fn a_password_only_server_cannot_be_tested_but_can_be_connected_to() {
        assert_eq!(
            render("password-only", Operation::Test),
            [
                "dev: failed \u{2014} the server accepts only a password",
                "  methods: password",
                "  next:    the test cannot type a password; `portico connect dev` will ask for it",
            ]
        );
        assert_eq!(
            render("password-only", Operation::Connect),
            [
                "dev: failed \u{2014} the server accepts only a password",
                "  methods: password",
            ]
        );
    }

    #[test]
    fn a_refusal_while_connecting_says_the_keys_were_not_reported() {
        assert_eq!(
            by_hand(
                Failure::AuthRefused {
                    methods: "publickey".into(),
                    offered: Vec::new(),
                    unusable: Vec::new(),
                },
                Operation::Connect
            ),
            [
                "dev: failed \u{2014} the server refused the login",
                "  user:    user@127.0.0.1:22999",
                "  offered: not reported at this log level",
                "  next:    check User and IdentityFile for dev, and that the public key is in the",
                "           server's authorized_keys",
                "           run `portico test dev` to see which keys were offered",
            ]
        );
    }

    #[test]
    fn a_name_that_does_not_resolve() {
        let home = home();
        let diagnosis = diagnose(&fixture("name-unresolved"), false, Route::Direct);
        let mut context = context(&home, Operation::Test, &[]);
        context.hostname = Some("dev");
        assert_eq!(
            explain(&diagnosis, &context).render("dev", false),
            [
                "dev: failed \u{2014} the host name does not resolve",
                "  name: probe.invalid (no HostName is set, so ssh looked up the alias)",
                "  next: check HostName for dev",
            ]
        );
        assert_eq!(
            render("name-unresolved", Operation::Test),
            [
                "dev: failed \u{2014} the host name does not resolve",
                "  name: probe.invalid",
                "  next: check HostName for dev",
            ]
        );
    }

    #[test]
    fn connection_problems_name_the_address() {
        assert_eq!(
            render("connection-refused", Operation::Test),
            [
                "dev: failed \u{2014} nothing accepted the connection",
                "  address: 127.0.0.1:22999",
                "  next:    check the port, and that an SSH server runs there",
            ]
        );
        assert_eq!(
            by_hand(Failure::Unreachable, Operation::Test),
            [
                "dev: failed \u{2014} the host is unreachable",
                "  address: 127.0.0.1:22999",
                "  next:    check the address and your network",
            ]
        );
    }

    #[test]
    fn a_timeout_says_which_limit_fired() {
        assert_eq!(
            by_hand(Failure::TimedOut { deadline: false }, Operation::Test),
            [
                "dev: failed \u{2014} the host did not answer in time",
                "  address: 127.0.0.1:22999",
                "  line:    no answer within 5 s",
                "  next:    check the address, the port and any firewall",
            ]
        );
        let lines = by_hand(Failure::TimedOut { deadline: true }, Operation::Test);
        assert_eq!(lines[2], "  line:    the test gave up after 15 s");
    }

    #[test]
    fn a_jump_failure_carries_the_jump_hosts_own_explanation() {
        let failure = Failure::JumpHostFailed {
            jump: "bastion".into(),
            cause: Box::new(Failure::ConnectionRefused),
        };
        assert_eq!(
            by_hand(failure.clone(), Operation::Test),
            [
                "dev: failed \u{2014} jump host bastion failed: nothing accepted the connection",
                "  next: check the port, and that an SSH server runs there",
            ]
        );
        let home = home();
        let diagnosis = Diagnosis {
            failure,
            raw: Vec::new(),
        };
        let mut context = context(&home, Operation::Test, &[]);
        context.jump_is_known = true;
        assert_eq!(
            explain(&diagnosis, &context).render("dev", false),
            [
                "dev: failed \u{2014} jump host bastion failed: nothing accepted the connection",
                "  next: run `portico test bastion` to check the jump host on its own",
                "        check the port, and that an SSH server runs there",
            ]
        );
    }

    #[test]
    fn a_jump_hosts_facts_sit_under_jump() {
        let failure = Failure::JumpHostFailed {
            jump: "bastion".into(),
            cause: Box::new(Failure::NameUnresolved {
                host: "bastion.invalid".into(),
            }),
        };
        assert_eq!(
            by_hand(failure, Operation::Test),
            [
                "dev: failed \u{2014} jump host bastion failed: the host name does not resolve",
                "  jump: name: bastion.invalid",
                "  next: check HostName for bastion",
            ]
        );
    }

    #[test]
    fn host_keys_are_untrusted_or_changed() {
        assert_eq!(
            by_hand(Failure::HostKeyUnknown, Operation::Test),
            ["dev: failed \u{2014} the host key is not trusted"]
        );
        assert_eq!(
            by_hand(Failure::HostKeyUnknown, Operation::Connect),
            [
                "dev: failed \u{2014} the host key is not trusted",
                "  next: the key was declined; review it with `portico test dev`",
            ]
        );
        assert_eq!(
            render("host-key-changed", Operation::Test),
            [
                "dev: failed \u{2014} the host key changed, which can mean an attack",
                "  file: /tmp/portico-fixture/kh_bad:1",
                "  next: once the new key is confirmed with the server's administrator, remove",
                "        the old one with `ssh-keygen -R 127.0.0.1`",
            ]
        );
    }

    #[test]
    fn a_rejected_configuration_names_the_file_and_line() {
        let user_file = Failure::ConfigRejected(ConfigRejection::UserFile {
            path: "/home/me/.ssh/config".into(),
            line: 12,
            message: "Bad configuration option: bogus".into(),
        });
        assert_eq!(
            by_hand(user_file, Operation::Test),
            [
                "dev: failed \u{2014} OpenSSH rejected the configuration",
                "  file: ~/.ssh/config",
                "  line: 12 (Bad configuration option: bogus)",
                "  next: fix that line, then try again",
            ]
        );
    }

    #[test]
    fn a_lost_connection_has_no_next_step() {
        assert_eq!(
            by_hand(
                Failure::ConnectionLost {
                    line: "Connection to h closed by remote host.".into()
                },
                Operation::Connect
            ),
            [
                "dev: failed \u{2014} the connection was lost",
                "  line: Connection to h closed by remote host.",
            ]
        );
    }

    #[test]
    fn an_unrecognised_failure_always_shows_what_ssh_said() {
        assert_eq!(
            render("other", Operation::Test),
            [
                "dev: failed \u{2014} command-line line 0: Bad SSH2 KexAlgorithms 'none-such'.",
                "  ssh said:",
                "    Unsupported KEX algorithm \"none-such\"",
                "    command-line line 0: Bad SSH2 KexAlgorithms 'none-such'.",
            ]
        );
    }

    #[test]
    fn raw_lines_follow_on_request() {
        let home = home();
        let diagnosis = diagnose(&fixture("password-only"), false, Route::Direct);
        let lines =
            explain(&diagnosis, &context(&home, Operation::Connect, &[])).render("dev", true);
        assert_eq!(
            lines[lines.len() - 2..],
            [
                "  ssh said:",
                "    user@127.0.0.1: Permission denied (password).",
            ]
        );
    }

    #[test]
    fn long_values_wrap_on_word_boundaries() {
        let explanation = Explanation {
            headline: "h".into(),
            facts: vec![(
                Label::Line,
                vec![format!("{} {}", "a".repeat(40), "b".repeat(40))],
            )],
            next: Vec::new(),
            raw: Vec::new(),
        };
        assert_eq!(
            explanation.render("x", false),
            [
                "x: failed \u{2014} h".to_string(),
                format!("  line: {}", "a".repeat(40)),
                format!("        {}", "b".repeat(40)),
            ]
        );
    }
}
