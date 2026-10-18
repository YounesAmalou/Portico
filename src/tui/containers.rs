//! The containers panel under the hosts: what it lists, its keys, and how it reads.

use std::{cell::Cell, collections::HashMap};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::{TextInput, TuiError, explanation_lines};
use crate::{
    app::{ContainerError, ContainerList, Offer, Target},
    container::{Container, GroupTool, display_name, group, remote_command},
    openssh::ConnectionOutcome,
};

/// Why a target could not be listed, as the panel shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PanelFailure {
    pub lines: Vec<String>,
    pub offer: Option<Offer>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PanelState {
    Unlisted,
    Listing,
    Listed(ContainerList),
    Failed(PanelFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PanelAction {
    None,
    Leave,
    Quit,
    List(Target),
    CancelList,
    Shell {
        target: Target,
        id: String,
        user: Option<String>,
        via: Option<GroupTool>,
        new_terminal: bool,
    },
    Fix {
        target: Target,
        offer: Offer,
    },
}

#[derive(Clone, Debug, Default)]
pub(super) struct Panel {
    pinned_local: bool,
    host: Option<String>,
    lists: HashMap<Target, PanelState>,
    selected: usize,
    user: TextInput,
    editing_user: bool,
    previous_user: String,
    /// The listing a running refresh replaces, which cancelling it restores.
    refreshing: HashMap<Target, PanelState>,
    /// The first line shown when the lines outgrow the panel, moved only as far as the
    /// selection needs.
    scroll: Cell<usize>,
}

impl Panel {
    /// This machine when pinned with `L`, otherwise the host the hosts panel has selected.
    pub(super) fn target(&self) -> Option<Target> {
        if self.pinned_local {
            Some(Target::Local)
        } else {
            self.host.clone().map(Target::Host)
        }
    }

    /// Follows the host selected in the hosts panel, which undoes an `L`.
    pub(super) fn follow(&mut self, alias: Option<&str>) {
        let before = self.target();
        self.host = alias.map(String::from);
        self.pinned_local = false;
        if self.target() != before {
            self.selected = 0;
            self.scroll.set(0);
        }
    }

    /// Follows the selected host only when it moved, so an `L` survives other keys.
    pub(super) fn follow_selection(&mut self, alias: Option<&str>) {
        if self.host.as_deref() != alias {
            self.follow(alias);
        }
    }

    /// Lists the target the first time the panel gains focus for it.
    pub(super) fn on_focus(&mut self) -> PanelAction {
        match self.target() {
            Some(target) if matches!(self.state(&target), PanelState::Unlisted) => {
                PanelAction::List(target)
            }
            _ => PanelAction::None,
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> PanelAction {
        if self.editing_user {
            match key.code {
                KeyCode::Enter => self.editing_user = false,
                KeyCode::Esc => {
                    self.editing_user = false;
                    self.user = TextInput::new(&self.previous_user);
                }
                _ => {
                    self.user.handle_key(key);
                }
            }
            return PanelAction::None;
        }
        let target = self.target();
        let state = target.as_ref().map(|target| self.state(target));
        match key.code {
            KeyCode::Esc if matches!(state, Some(PanelState::Listing)) => {
                if let Some(target) = target {
                    self.abandon(target);
                }
                PanelAction::CancelList
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => PanelAction::Leave,
            KeyCode::Char('q') => PanelAction::Quit,
            KeyCode::Char('L') => {
                if !self.pinned_local {
                    self.pinned_local = true;
                    self.selected = 0;
                }
                self.on_focus()
            }
            KeyCode::Char('r') => target.map_or(PanelAction::None, PanelAction::List),
            KeyCode::Char('u') => {
                self.previous_user = self.user.as_str().to_owned();
                self.editing_user = true;
                PanelAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                PanelAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let count = self.rows().len();
                if self.selected + 1 < count {
                    self.selected += 1;
                }
                PanelAction::None
            }
            KeyCode::Enter => self.shell(false),
            KeyCode::Char('n') => self.shell(true),
            KeyCode::Char(character @ ('g' | 's')) => match (state, target) {
                (Some(PanelState::Failed(failure)), Some(target)) => match &failure.offer {
                    Some(offer @ Offer::JoinGroup { .. }) if character == 'g' => PanelAction::Fix {
                        target,
                        offer: offer.clone(),
                    },
                    Some(offer @ Offer::StartDaemon) if character == 's' => PanelAction::Fix {
                        target,
                        offer: offer.clone(),
                    },
                    _ => PanelAction::None,
                },
                _ => PanelAction::None,
            },
            _ => PanelAction::None,
        }
    }

    pub(super) fn set_listing(&mut self, target: Target) {
        if let Some(previous @ PanelState::Listed(_)) =
            self.lists.insert(target.clone(), PanelState::Listing)
        {
            self.refreshing.insert(target, previous);
        }
    }

    /// Forgets a listing that will not finish: a refreshed listing comes back, anything else
    /// is listed again on the next focus.
    pub(super) fn abandon(&mut self, target: Target) {
        match self.refreshing.remove(&target) {
            Some(previous) => self.lists.insert(target, previous),
            None => self.lists.remove(&target),
        };
    }

    /// Keeps a listing's result for the session.
    pub(super) fn finish(&mut self, target: Target, result: Result<ContainerList, ContainerError>) {
        let state = match result {
            Ok(list) => PanelState::Listed(list),
            Err(error) => PanelState::Failed(failure(&error)),
        };
        self.refreshing.remove(&target);
        self.lists.insert(target, state);
        let count = self.rows().len();
        self.selected = self.selected.min(count.saturating_sub(1));
    }

    pub(super) fn title(&self) -> String {
        match self.target() {
            Some(target) => format!("Containers: {target}"),
            None => "Containers".into(),
        }
    }

    /// The panel's lines for `height` rows, scrolled so the selected row shows, with the user
    /// line kept last while focused.
    pub(super) fn lines(&self, focused: bool, height: usize) -> Vec<Line<'static>> {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let (mut lines, selected) = self.body();
        let room = height.saturating_sub(usize::from(focused));
        if lines.len() > room {
            let mut first = self.scroll.get();
            match selected {
                Some(line) if line < first => first = line,
                Some(line) if line >= first + room => first = line + 1 - room,
                Some(_) => {}
                None => first = 0,
            }
            first = first.min(lines.len() - room);
            self.scroll.set(first);
            lines = lines.into_iter().skip(first).take(room).collect();
        }
        if focused {
            let mut user = vec![Span::raw("user: ")];
            if self.editing_user {
                user.extend(self.user.spans(true));
            } else if let Some(chosen) = self.chosen_user() {
                user.push(Span::raw(chosen));
            } else {
                user.push(Span::styled("image default", dim));
            }
            lines.push(Line::from(user));
        }
        lines
    }

    /// Every line above the user line, and the index of the selected row's line.
    fn body(&self) -> (Vec<Line<'static>>, Option<usize>) {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let Some(target) = self.target() else {
            return (vec![Line::raw("No host selected")], None);
        };
        let mut selected = None;
        let lines = match self.state(&target) {
            PanelState::Unlisted => vec![Line::raw("Tab to list containers")],
            PanelState::Listing => vec![Line::raw("Listing…")],
            PanelState::Failed(failure) => {
                let mut lines: Vec<Line> = failure.lines.iter().cloned().map(Line::raw).collect();
                match failure.offer {
                    Some(Offer::JoinGroup { .. }) => lines.push(Line::raw("g add to docker group")),
                    Some(Offer::StartDaemon) => lines.push(Line::raw("s start docker")),
                    None => {}
                }
                lines
            }
            PanelState::Listed(list) => {
                let mut lines = Vec::new();
                if let (Some(_), Some(note)) = (list.via, &list.note) {
                    lines.push(Line::styled(note.clone(), dim));
                }
                if list.listing.skipped > 0 {
                    lines.push(Line::styled(
                        format!(
                            "Skipped {} lines docker printed that Portico could not read.",
                            list.listing.skipped
                        ),
                        dim,
                    ));
                }
                let all = &list.listing.containers;
                if all.is_empty() {
                    lines.push(Line::raw(format!("No running containers on {target}.")));
                }
                let mut row = 0;
                for group in group(all) {
                    lines.push(Line::styled(
                        group.project.unwrap_or("Other").to_owned(),
                        bold,
                    ));
                    for container in group.containers {
                        let mark = if row == self.selected {
                            selected = Some(lines.len());
                            ">"
                        } else {
                            " "
                        };
                        lines.push(Line::from(vec![
                            Span::raw(format!("{mark} {}  ", display_name(container, all))),
                            Span::styled(container.status.clone(), dim),
                        ]));
                        row += 1;
                    }
                }
                lines
            }
        };
        (lines, selected)
    }

    /// The selected container, one `label: value` line per non-empty field.
    pub(super) fn details(&self) -> Option<String> {
        let container = *self.rows().get(self.selected)?;
        let chosen = self.chosen_user();
        let user = chosen.as_deref().unwrap_or("image default");
        let rows = [
            ("name", container.name.as_str()),
            ("id", &container.id),
            ("image", &container.image),
            ("status", &container.status),
            ("running for", &container.running_for),
            ("ports", &container.ports),
            ("project", container.project.as_deref().unwrap_or_default()),
            ("service", container.service.as_deref().unwrap_or_default()),
            (
                "working dir",
                container.working_dir.as_deref().unwrap_or_default(),
            ),
            ("networks", &container.networks),
            ("mounts", &container.mounts),
            ("command", &container.command),
            ("user", user),
        ];
        Some(
            rows.iter()
                .filter(|(_, value)| !value.is_empty())
                .map(|(label, value)| format!("{label}: {value}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    pub(super) fn footer(&self) -> &'static str {
        if self.editing_user {
            "Enter apply  Esc cancel"
        } else {
            "↑↓ choose  Enter shell  n new terminal  u user  r refresh  L this machine  Tab hosts"
        }
    }

    fn state(&self, target: &Target) -> &PanelState {
        self.lists.get(target).unwrap_or(&PanelState::Unlisted)
    }

    fn listed(&self) -> Option<&ContainerList> {
        match self.state(&self.target()?) {
            PanelState::Listed(list) => Some(list),
            _ => None,
        }
    }

    /// The containers of the current listing in the order they are shown, headers left out.
    fn rows(&self) -> Vec<&Container> {
        self.listed().map_or_else(Vec::new, |list| {
            group(&list.listing.containers)
                .into_iter()
                .flat_map(|group| group.containers)
                .collect()
        })
    }

    /// The user for the next shell, trimmed; blank means the image's own.
    fn chosen_user(&self) -> Option<String> {
        let user = self.user.as_str().trim();
        (!user.is_empty()).then(|| user.to_owned())
    }

    fn shell(&self, new_terminal: bool) -> PanelAction {
        let (Some(list), Some(container)) =
            (self.listed(), self.rows().get(self.selected).copied())
        else {
            return PanelAction::None;
        };
        PanelAction::Shell {
            target: list.target.clone(),
            id: container.id.clone(),
            user: self.chosen_user(),
            via: list.via,
            new_terminal,
        }
    }
}

/// The status line after a shell on `target`.
pub(super) fn shell_status(
    target: &Target,
    result: Result<Result<ConnectionOutcome, ContainerError>, TuiError>,
) -> String {
    match result {
        Ok(Ok(ConnectionOutcome::Exited {
            explanation: Some(explanation),
            ..
        })) => explanation.headline,
        Ok(Ok(ConnectionOutcome::Exited { status, .. })) if status.success() => {
            format!("Shell on {target} closed")
        }
        Ok(Ok(ConnectionOutcome::Exited { status, .. })) => {
            format!("Shell on {target} exited with {status}")
        }
        Ok(Ok(ConnectionOutcome::LaunchRequested)) => {
            format!("Opened a shell on {target} in a new terminal")
        }
        Ok(Ok(ConnectionOutcome::LaunchFailed(status))) => {
            format!("Could not open a terminal: {status}")
        }
        Ok(Err(error)) => error.to_string(),
        Err(error) => error.to_string(),
    }
}

/// The status line after running `offer`'s fix: nothing when it succeeded.
pub(super) fn fix_status(
    offer: &Offer,
    result: Result<Result<std::process::ExitStatus, ContainerError>, TuiError>,
) -> Option<String> {
    match result {
        Ok(Ok(status)) if status.success() => None,
        Ok(Ok(status)) => Some(format!(
            "{} exited with {status}",
            remote_command(&offer.words())
        )),
        Ok(Err(error)) => Some(error.to_string()),
        Err(error) => Some(error.to_string()),
    }
}

/// The panel's wording of a failed listing.
fn failure(error: &ContainerError) -> PanelFailure {
    match error {
        ContainerError::Ssh {
            explanation: Some(explanation),
            ..
        } => PanelFailure {
            lines: std::iter::once(format!("{error} \u{2014} {}", explanation.headline))
                .chain(explanation_lines(explanation, false))
                .collect(),
            offer: None,
        },
        ContainerError::Ssh { stderr, .. } => PanelFailure {
            lines: std::iter::once(error.to_string())
                .chain(
                    stderr
                        .lines()
                        .map(str::trim)
                        .rfind(|line| !line.is_empty())
                        .map(String::from),
                )
                .collect(),
            offer: None,
        },
        ContainerError::Docker(issue) => PanelFailure {
            lines: std::iter::once(issue.headline.clone())
                .chain(issue.next.iter().cloned())
                .collect(),
            offer: issue.offer.clone(),
        },
        other => PanelFailure {
            lines: vec![other.to_string()],
            offer: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use crate::{
        app::DockerIssue,
        container::{DockerProblem, Listing},
        explain::{Explanation, Label},
    };

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(panel: &mut Panel, code: KeyCode) -> PanelAction {
        panel.handle_key(key(code))
    }

    fn container(id: &str, name: &str, project: Option<&str>, service: Option<&str>) -> Container {
        Container {
            id: id.into(),
            name: name.into(),
            image: "nginx:1.27".into(),
            command: "\"nginx -g 'daemon off;'\"".into(),
            running_for: "2 hours ago".into(),
            status: "Up 2 hours".into(),
            ports: String::new(),
            networks: "shop_default".into(),
            mounts: String::new(),
            project: project.map(String::from),
            service: service.map(String::from),
            number: project.map(|_| 1),
            working_dir: project.map(|project| format!("/srv/{project}")),
        }
    }

    /// Two Compose projects and a plain container: rows `api`, `web` under `shop`, `db` under
    /// `wiki`, then `redis` under `Other`.
    fn containers() -> Vec<Container> {
        vec![
            container("bbbb", "shop-web-1", Some("shop"), Some("web")),
            container("cccc", "wiki-db-1", Some("wiki"), Some("db")),
            container("aaaa", "shop-api-1", Some("shop"), Some("api")),
            container("dddd", "redis", None, None),
        ]
    }

    fn list(target: Target, containers: Vec<Container>) -> ContainerList {
        ContainerList {
            target,
            listing: Listing {
                containers,
                skipped: 0,
            },
            via: None,
            note: None,
        }
    }

    fn host(alias: &str) -> Target {
        Target::Host(alias.into())
    }

    /// A panel following `dev`, listed with [`containers`].
    fn listed_panel() -> Panel {
        let mut panel = Panel::default();
        panel.follow(Some("dev"));
        panel.finish(host("dev"), Ok(list(host("dev"), containers())));
        panel
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    fn issue(offer: Option<Offer>) -> ContainerError {
        ContainerError::from(DockerIssue {
            target: host("dev"),
            problem: DockerProblem::DaemonDown,
            headline: "The docker daemon is not running on dev.".into(),
            next: vec!["Start it with: sudo systemctl start docker".into()],
            offer,
            stderr: "Cannot connect to the Docker daemon".into(),
        })
    }

    #[test]
    fn focus_lists_once_per_target() {
        let mut panel = Panel::default();
        panel.follow(Some("dev"));
        assert_eq!(panel.on_focus(), PanelAction::List(host("dev")));
        panel.set_listing(host("dev"));
        assert_eq!(panel.on_focus(), PanelAction::None, "already listing");
        panel.finish(host("dev"), Ok(list(host("dev"), containers())));
        assert_eq!(panel.on_focus(), PanelAction::None);

        panel.follow(Some("web"));
        assert_eq!(panel.on_focus(), PanelAction::List(host("web")));
        panel.follow(Some("dev"));
        assert_eq!(panel.on_focus(), PanelAction::None, "kept for the session");
    }

    #[test]
    fn follow_unpins_local() {
        let mut panel = Panel::default();
        assert_eq!(panel.target(), None);
        assert_eq!(panel.title(), "Containers");
        panel.follow(Some("dev"));
        assert_eq!(panel.target(), Some(host("dev")));

        assert_eq!(
            press(&mut panel, KeyCode::Char('L')),
            PanelAction::List(Target::Local)
        );
        assert_eq!(panel.target(), Some(Target::Local));
        assert_eq!(panel.title(), "Containers: this machine");

        panel.follow(Some("web"));
        assert_eq!(panel.target(), Some(host("web")));
        assert_eq!(panel.title(), "Containers: web");
    }

    #[test]
    fn selection_skips_headers() {
        let mut panel = listed_panel();
        let selected = |panel: &Panel| {
            texts(&panel.lines(true, 100))
                .into_iter()
                .filter(|line| line.starts_with('>'))
                .collect::<Vec<_>>()
        };
        assert_eq!(selected(&panel), ["> api  Up 2 hours"]);
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Char('j'));
        assert_eq!(
            selected(&panel),
            ["> db  Up 2 hours"],
            "past the wiki header"
        );
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Down);
        assert_eq!(
            selected(&panel),
            ["> redis  Up 2 hours"],
            "stops at the end"
        );
        press(&mut panel, KeyCode::Char('k'));
        press(&mut panel, KeyCode::Up);
        press(&mut panel, KeyCode::Up);
        press(&mut panel, KeyCode::Up);
        assert_eq!(
            selected(&panel),
            ["> api  Up 2 hours"],
            "stops at the start"
        );

        press(&mut panel, KeyCode::Down);
        panel.follow(Some("web"));
        panel.finish(host("web"), Ok(list(host("web"), containers())));
        assert_eq!(
            selected(&panel),
            ["> api  Up 2 hours"],
            "reset on a new target"
        );
    }

    #[test]
    fn enter_and_n_produce_shell_with_user() {
        let mut panel = listed_panel();
        assert_eq!(
            press(&mut panel, KeyCode::Enter),
            PanelAction::Shell {
                target: host("dev"),
                id: "aaaa".into(),
                user: None,
                via: None,
                new_terminal: false,
            }
        );

        assert_eq!(press(&mut panel, KeyCode::Char('u')), PanelAction::None);
        assert_eq!(panel.footer(), "Enter apply  Esc cancel");
        for character in "root".chars() {
            assert_eq!(
                press(&mut panel, KeyCode::Char(character)),
                PanelAction::None
            );
        }
        assert_eq!(
            press(&mut panel, KeyCode::Enter),
            PanelAction::None,
            "applies"
        );
        assert_eq!(texts(&panel.lines(true, 100)).last().unwrap(), "user: root");

        press(&mut panel, KeyCode::Down);
        assert_eq!(
            press(&mut panel, KeyCode::Char('n')),
            PanelAction::Shell {
                target: host("dev"),
                id: "bbbb".into(),
                user: Some("root".into()),
                via: None,
                new_terminal: true,
            }
        );
    }

    #[test]
    fn esc_while_editing_user_restores() {
        let mut panel = listed_panel();
        assert_eq!(
            texts(&panel.lines(true, 100)).last().unwrap(),
            "user: image default"
        );
        press(&mut panel, KeyCode::Char('u'));
        press(&mut panel, KeyCode::Char('x'));
        assert_eq!(
            press(&mut panel, KeyCode::Char('q')),
            PanelAction::None,
            "typed"
        );
        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::None, "stays");
        assert_eq!(
            texts(&panel.lines(true, 100)).last().unwrap(),
            "user: image default"
        );
        assert_eq!(
            panel.footer(),
            "↑↓ choose  Enter shell  n new terminal  u user  r refresh  L this machine  Tab hosts"
        );
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            PanelAction::Shell { user: None, .. }
        ));
        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::Leave);
        assert_eq!(press(&mut panel, KeyCode::Tab), PanelAction::Leave);
        assert_eq!(press(&mut panel, KeyCode::BackTab), PanelAction::Leave);
        assert_eq!(
            press(&mut panel, KeyCode::Char('r')),
            PanelAction::List(host("dev"))
        );
        assert_eq!(press(&mut panel, KeyCode::Char('q')), PanelAction::Quit);
    }

    #[test]
    fn esc_while_listing_cancels() {
        let mut panel = Panel::default();
        panel.follow(Some("dev"));
        panel.set_listing(host("dev"));
        assert_eq!(texts(&panel.lines(true, 100))[0], "Listing…");
        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::CancelList);
        assert_eq!(
            panel.on_focus(),
            PanelAction::List(host("dev")),
            "lists again"
        );
    }

    #[test]
    fn offer_keys_only_for_matching_offer() {
        let mut panel = Panel::default();
        panel.follow(Some("dev"));
        panel.finish(host("dev"), Err(issue(Some(Offer::StartDaemon))));
        assert_eq!(press(&mut panel, KeyCode::Char('g')), PanelAction::None);
        assert_eq!(
            press(&mut panel, KeyCode::Char('s')),
            PanelAction::Fix {
                target: host("dev"),
                offer: Offer::StartDaemon,
            }
        );

        let join = Offer::JoinGroup {
            user: "deploy".into(),
            socket: "/var/run/docker.sock".into(),
        };
        panel.finish(host("dev"), Err(issue(Some(join.clone()))));
        assert_eq!(press(&mut panel, KeyCode::Char('s')), PanelAction::None);
        assert_eq!(
            press(&mut panel, KeyCode::Char('g')),
            PanelAction::Fix {
                target: host("dev"),
                offer: join,
            }
        );

        panel.finish(host("dev"), Err(issue(None)));
        assert_eq!(press(&mut panel, KeyCode::Char('g')), PanelAction::None);
        assert_eq!(press(&mut panel, KeyCode::Char('s')), PanelAction::None);
        assert_eq!(press(&mut panel, KeyCode::Enter), PanelAction::None);
    }

    #[test]
    fn renders_groups_and_hint() {
        let mut panel = Panel::default();
        assert_eq!(texts(&panel.lines(false, 100)), ["No host selected"]);
        panel.follow(Some("dev"));
        assert_eq!(texts(&panel.lines(false, 100)), ["Tab to list containers"]);

        let mut listed = list(host("dev"), containers());
        listed.via = Some(GroupTool::Sg);
        listed.note = Some("Listed through sg: other shells still lack the docker group.".into());
        listed.listing.skipped = 2;
        panel.finish(host("dev"), Ok(listed));
        assert_eq!(
            texts(&panel.lines(false, 100)),
            [
                "Listed through sg: other shells still lack the docker group.",
                "Skipped 2 lines docker printed that Portico could not read.",
                "shop",
                "> api  Up 2 hours",
                "  web  Up 2 hours",
                "wiki",
                "  db  Up 2 hours",
                "Other",
                "  redis  Up 2 hours",
            ]
        );
        assert_eq!(
            texts(&panel.lines(true, 100)).last().unwrap(),
            "user: image default"
        );

        panel.finish(host("dev"), Ok(list(host("dev"), Vec::new())));
        assert_eq!(
            texts(&panel.lines(false, 100)),
            ["No running containers on dev."]
        );
        assert_eq!(press(&mut panel, KeyCode::Enter), PanelAction::None);
    }

    #[test]
    fn details_for_selected_container() {
        let panel = Panel::default();
        assert_eq!(panel.details(), None);
        let mut panel = listed_panel();
        press(&mut panel, KeyCode::Down);
        assert_eq!(
            panel.details().unwrap(),
            "name: shop-web-1\n\
             id: bbbb\n\
             image: nginx:1.27\n\
             status: Up 2 hours\n\
             running for: 2 hours ago\n\
             project: shop\n\
             service: web\n\
             working dir: /srv/shop\n\
             networks: shop_default\n\
             command: \"nginx -g 'daemon off;'\"\n\
             user: image default"
        );
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Char('u'));
        for character in "root".chars() {
            press(&mut panel, KeyCode::Char(character));
        }
        press(&mut panel, KeyCode::Enter);
        assert_eq!(
            panel.details().unwrap(),
            "name: redis\n\
             id: dddd\n\
             image: nginx:1.27\n\
             status: Up 2 hours\n\
             running for: 2 hours ago\n\
             networks: shop_default\n\
             command: \"nginx -g 'daemon off;'\"\n\
             user: root"
        );
    }

    #[test]
    fn failure_lines_for_ssh_and_docker() {
        let mut panel = Panel::default();
        panel.follow(Some("dev"));
        panel.finish(
            host("dev"),
            Err(ContainerError::Ssh {
                alias: "dev".into(),
                explanation: Some(Box::new(Explanation {
                    headline: "the connection was refused".into(),
                    facts: vec![(Label::User, vec!["deploy".into()])],
                    next: vec!["check sshd".into()],
                    raw: vec!["ssh: connect to host dev port 22: Connection refused".into()],
                    always_raw: false,
                })),
                stderr: "ssh: connect to host dev port 22: Connection refused\n".into(),
            }),
        );
        assert_eq!(
            texts(&panel.lines(false, 100)),
            [
                "ssh to dev failed \u{2014} the connection was refused",
                "  user: deploy",
                "  next: check sshd",
            ]
        );

        panel.finish(
            host("dev"),
            Err(ContainerError::Ssh {
                alias: "dev".into(),
                explanation: None,
                stderr: "banner\nssh: Could not resolve hostname dev\n\n".into(),
            }),
        );
        assert_eq!(
            texts(&panel.lines(false, 100)),
            ["ssh to dev failed", "ssh: Could not resolve hostname dev"]
        );

        panel.finish(host("dev"), Err(issue(Some(Offer::StartDaemon))));
        assert_eq!(
            texts(&panel.lines(false, 100)),
            [
                "The docker daemon is not running on dev.",
                "Start it with: sudo systemctl start docker",
                "s start docker",
            ]
        );

        let join = Offer::JoinGroup {
            user: "deploy".into(),
            socket: "/var/run/docker.sock".into(),
        };
        panel.finish(host("dev"), Err(issue(Some(join))));
        assert_eq!(
            texts(&panel.lines(false, 100)).last().unwrap(),
            "g add to docker group"
        );

        panel.finish(
            host("dev"),
            Err(ContainerError::NoMatch {
                target: host("dev"),
                reference: "web".into(),
            }),
        );
        assert_eq!(
            texts(&panel.lines(false, 100)),
            ["no container matches web on dev"]
        );
    }

    #[test]
    fn esc_during_refresh_restores_listing() {
        let mut panel = listed_panel();
        let before = texts(&panel.lines(false, 100));
        assert_eq!(
            press(&mut panel, KeyCode::Char('r')),
            PanelAction::List(host("dev"))
        );
        panel.set_listing(host("dev"));
        assert_eq!(texts(&panel.lines(false, 100)), ["Listing…"]);
        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::CancelList);
        assert_eq!(texts(&panel.lines(false, 100)), before);
        assert_eq!(panel.on_focus(), PanelAction::None);

        panel.set_listing(host("dev"));
        panel.finish(host("dev"), Ok(list(host("dev"), Vec::new())));
        panel.set_listing(host("dev"));
        panel.finish(host("dev"), Err(issue(None)));
        panel.set_listing(host("dev"));
        press(&mut panel, KeyCode::Esc);
        assert_eq!(
            panel.on_focus(),
            PanelAction::List(host("dev")),
            "a failure is not restored"
        );
    }

    #[test]
    fn blank_user_means_image_default() {
        let mut panel = listed_panel();
        press(&mut panel, KeyCode::Char('u'));
        press(&mut panel, KeyCode::Char(' '));
        press(&mut panel, KeyCode::Char(' '));
        press(&mut panel, KeyCode::Enter);
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            PanelAction::Shell { user: None, .. }
        ));
        assert!(panel.details().unwrap().ends_with("user: image default"));
        assert_eq!(
            texts(&panel.lines(true, 100)).last().unwrap(),
            "user: image default"
        );

        press(&mut panel, KeyCode::Char('u'));
        for character in "root ".chars() {
            press(&mut panel, KeyCode::Char(character));
        }
        press(&mut panel, KeyCode::Enter);
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            PanelAction::Shell { user: Some(user), .. } if user == "root"
        ));
        assert!(panel.details().unwrap().ends_with("user: root"));
    }

    #[test]
    fn long_listing_scrolls_to_selection() {
        let mut panel = listed_panel();
        let shown = |panel: &Panel| texts(&panel.lines(true, 4));
        assert_eq!(
            shown(&panel),
            [
                "shop",
                "> api  Up 2 hours",
                "  web  Up 2 hours",
                "user: image default"
            ]
        );
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Down);
        assert_eq!(
            shown(&panel),
            [
                "  db  Up 2 hours",
                "Other",
                "> redis  Up 2 hours",
                "user: image default"
            ]
        );
        press(&mut panel, KeyCode::Up);
        assert_eq!(
            shown(&panel)[..3],
            ["> db  Up 2 hours", "Other", "  redis  Up 2 hours"],
            "no jump"
        );
        press(&mut panel, KeyCode::Up);
        assert_eq!(
            shown(&panel)[..3],
            ["> web  Up 2 hours", "wiki", "  db  Up 2 hours"]
        );
        assert_eq!(texts(&panel.lines(false, 2)), ["> web  Up 2 hours", "wiki"]);
        assert_eq!(texts(&panel.lines(true, 0)), ["user: image default"]);
    }

    #[test]
    fn shell_and_fix_status_lines() {
        use std::os::unix::process::ExitStatusExt;
        let ok = std::process::ExitStatus::from_raw(0);
        let failed = std::process::ExitStatus::from_raw(256);
        let exited = |status, explanation| {
            Ok(Ok(ConnectionOutcome::Exited {
                alias: "dev".into(),
                status,
                explanation,
            }))
        };
        let dev = host("dev");
        assert_eq!(shell_status(&dev, exited(ok, None)), "Shell on dev closed");
        assert_eq!(
            shell_status(&dev, exited(failed, None)),
            "Shell on dev exited with exit status: 1"
        );
        let explanation = Explanation {
            headline: "the connection was refused".into(),
            facts: Vec::new(),
            next: Vec::new(),
            raw: Vec::new(),
            always_raw: false,
        };
        assert_eq!(
            shell_status(&dev, exited(failed, Some(explanation))),
            "the connection was refused"
        );
        assert_eq!(
            shell_status(&Target::Local, Ok(Ok(ConnectionOutcome::LaunchRequested))),
            "Opened a shell on this machine in a new terminal"
        );
        assert_eq!(
            shell_status(&dev, Ok(Ok(ConnectionOutcome::LaunchFailed(failed)))),
            "Could not open a terminal: exit status: 1"
        );
        assert_eq!(
            shell_status(&dev, Ok(Err(issue(None)))),
            "The docker daemon is not running on dev."
        );

        assert_eq!(fix_status(&Offer::StartDaemon, Ok(Ok(ok))), None);
        let line = fix_status(&Offer::StartDaemon, Ok(Ok(failed))).unwrap();
        assert_eq!(
            line,
            format!(
                "{} exited with exit status: 1",
                remote_command(&Offer::StartDaemon.words())
            )
        );
        assert!(line.contains("sudo"), "{line}");
    }

    #[test]
    fn abandoning_a_superseded_listing() {
        let mut panel = listed_panel();
        panel.set_listing(host("dev"));
        panel.follow(Some("web"));
        panel.set_listing(host("web"));
        panel.abandon(host("dev"));
        panel.abandon(host("web"));
        assert_eq!(panel.on_focus(), PanelAction::List(host("web")));
        panel.follow(Some("dev"));
        assert_eq!(panel.on_focus(), PanelAction::None);
        assert_eq!(texts(&panel.lines(false, 100))[1], "> api  Up 2 hours");
    }
}
