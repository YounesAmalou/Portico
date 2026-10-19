//! The containers panel under the hosts: what it lists, its keys, and how it reads.

use std::{cell::Cell, collections::HashMap};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::{TextInput, TuiError, display_width, explanation_lines};
use crate::{
    app::{ContainerError, ContainerList, Offer, Target},
    container::{Container, GroupTool, display_name, group, remote_command},
    fuzzy,
    openssh::ConnectionOutcome,
    picker::HIT,
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
    /// The host whose containers are shown, or this machine when `None`.
    host: Option<String>,
    lists: HashMap<Target, PanelState>,
    selected: usize,
    user: TextInput,
    editing_user: bool,
    previous_user: String,
    filter: TextInput,
    editing_filter: bool,
    /// The filter as it was when its edit began, which Esc restores.
    filter_before_edit: String,
    /// The listing a running refresh replaces, which cancelling it restores.
    refreshing: HashMap<Target, PanelState>,
    /// The first line shown when the lines outgrow the panel, moved only as far as the
    /// selection needs.
    scroll: Cell<usize>,
}

impl Panel {
    /// The machine whose containers the panel shows: this one until `c` picks a host.
    pub(super) fn target(&self) -> Target {
        self.host.clone().map_or(Target::Local, Target::Host)
    }

    /// Shows `target`'s containers, listing them unless a listing is kept or running. Another
    /// target starts unfiltered at its first row.
    pub(super) fn show(&mut self, target: Target) -> PanelAction {
        if target != self.target() {
            self.host = match &target {
                Target::Local => None,
                Target::Host(alias) => Some(alias.clone()),
            };
            self.selected = 0;
            self.scroll.set(0);
            self.filter = TextInput::default();
            self.editing_filter = false;
        }
        match self.state(&target) {
            PanelState::Unlisted | PanelState::Failed(_) => PanelAction::List(target),
            PanelState::Listing | PanelState::Listed(_) => PanelAction::None,
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> PanelAction {
        if self.editing_filter {
            match key.code {
                KeyCode::Enter => self.editing_filter = false,
                KeyCode::Esc => {
                    self.editing_filter = false;
                    let before = std::mem::take(&mut self.filter_before_edit);
                    self.set_filter(TextInput::new(&before));
                }
                _ => {
                    let mut filter = self.filter.clone();
                    if filter.handle_key(key) {
                        self.set_filter(filter);
                    }
                }
            }
            return PanelAction::None;
        }
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
        let state = self.state(&target);
        match key.code {
            KeyCode::Esc if matches!(state, PanelState::Listing) => {
                self.abandon(target);
                PanelAction::CancelList
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.set_filter(TextInput::default());
                PanelAction::None
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => PanelAction::Leave,
            KeyCode::Char('q') => PanelAction::Quit,
            KeyCode::Char('L') => self.show(Target::Local),
            KeyCode::Char('r') => PanelAction::List(target),
            KeyCode::Char('/') => {
                self.filter_before_edit = self.filter.as_str().to_owned();
                self.filter.cursor = self.filter.as_str().len();
                self.editing_filter = true;
                PanelAction::None
            }
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
            KeyCode::Char(character @ ('g' | 's')) => match state {
                PanelState::Failed(failure) => match &failure.offer {
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

    /// Pastes into the filter or the user while it is being edited; otherwise a paste means
    /// nothing here.
    pub(super) fn paste(&mut self, text: &str) {
        if self.editing_filter {
            let mut filter = self.filter.clone();
            filter.paste(text);
            self.set_filter(filter);
        } else if self.editing_user {
            self.user.paste(text);
        }
    }

    /// Applies a new filter, keeping the selected container when it still matches and
    /// otherwise selecting the first match.
    fn set_filter(&mut self, filter: TextInput) {
        let selected = self.rows().get(self.selected).map(|row| row.id.clone());
        self.filter = filter;
        self.selected = selected
            .and_then(|id| self.rows().iter().position(|row| row.id == id))
            .unwrap_or(0);
    }

    /// Whether `container` matches the filter on the name the panel shows, its name, Compose
    /// project and service, or image, ignoring case and accents.
    fn matches(&self, container: &Container, all: &[Container]) -> bool {
        self.shown_match(container, all).is_some()
    }

    /// The match of the filter on `container`, with the matched characters of the name the
    /// panel shows first among its positions.
    fn shown_match(&self, container: &Container, all: &[Container]) -> Option<fuzzy::Match> {
        let shown = display_name(container, all);
        fuzzy::match_fields(
            self.filter.as_str(),
            &[
                &shown,
                &container.name,
                container.project.as_deref().unwrap_or_default(),
                container.service.as_deref().unwrap_or_default(),
                &container.image,
            ],
        )
    }

    /// Shows that `target` could not be listed for a reason of Portico's own.
    pub(super) fn fail(&mut self, target: Target, line: &str) {
        self.refreshing.remove(&target);
        self.lists.insert(
            target,
            PanelState::Failed(PanelFailure {
                lines: vec![line.to_owned()],
                offer: None,
            }),
        );
    }

    /// Forgets a listing that will not finish: a refreshed listing comes back, anything else
    /// is left unlisted until `r`, `c` or `L` lists it.
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

    /// `Containers: <target> /<filter>`, with the filter's cursor while it is edited.
    pub(super) fn title(&self) -> Line<'static> {
        let mut title = vec![Span::raw(format!("Containers: {} /", self.target()))];
        title.extend(self.filter.spans(self.editing_filter));
        Line::from(title)
    }

    /// The panel's lines for `height` rows of `width` columns, scrolled so the selected row
    /// shows, with the user line kept last while focused. Messages wrap to the width; container
    /// rows are cut at the edge instead, so each stays one line.
    pub(super) fn lines(&self, focused: bool, height: usize, width: usize) -> Vec<Line<'static>> {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let (mut lines, selected) = self.body(width);
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
    fn body(&self, width: usize) -> (Vec<Line<'static>>, Option<usize>) {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let wrapped = |text: &str, style: Style| {
            wrap(text, width)
                .into_iter()
                .map(move |line| Line::styled(line, style))
        };
        let target = self.target();
        let mut selected = None;
        let lines = match self.state(&target) {
            PanelState::Unlisted => vec![Line::raw("Not listed: r lists containers")],
            PanelState::Listing => vec![Line::raw("Listing…")],
            PanelState::Failed(failure) => {
                let mut lines: Vec<Line> = failure
                    .lines
                    .iter()
                    .flat_map(|line| wrapped(line, Style::default()))
                    .collect();
                let hint = match failure.offer {
                    Some(Offer::JoinGroup { .. }) => Some("g add to docker group"),
                    Some(Offer::StartDaemon) => Some("s start docker"),
                    None => None,
                };
                lines.extend(
                    hint.into_iter()
                        .flat_map(|hint| wrapped(hint, Style::default())),
                );
                lines
            }
            PanelState::Listed(list) => {
                let mut lines = Vec::new();
                if let (Some(_), Some(note)) = (list.via, &list.note) {
                    lines.extend(wrapped(note, dim));
                }
                if list.listing.skipped > 0 {
                    lines.extend(wrapped(
                        &format!(
                            "Skipped {} lines docker printed that Portico could not read.",
                            list.listing.skipped
                        ),
                        dim,
                    ));
                }
                let all = &list.listing.containers;
                if all.is_empty() {
                    lines.extend(wrapped(
                        &format!("No running containers on {target}."),
                        Style::default(),
                    ));
                }
                let mut row = 0;
                for group in group(all) {
                    let shown: Vec<&Container> = group
                        .containers
                        .into_iter()
                        .filter(|container| self.matches(container, all))
                        .collect();
                    if shown.is_empty() {
                        continue;
                    }
                    lines.push(Line::styled(
                        group.project.unwrap_or("Other").to_owned(),
                        bold,
                    ));
                    for container in shown {
                        let mark = if row == self.selected {
                            selected = Some(lines.len());
                            ">"
                        } else {
                            " "
                        };
                        let positions = self
                            .shown_match(container, all)
                            .and_then(|found| found.positions.into_iter().next())
                            .unwrap_or_default();
                        let mut spans = vec![Span::raw(format!("{mark} "))];
                        spans.extend(fuzzy::highlight(
                            &display_name(container, all),
                            &positions,
                            Style::default(),
                            HIT,
                        ));
                        spans.push(Span::raw("  "));
                        spans.push(Span::styled(container.status.clone(), dim));
                        lines.push(Line::from(spans));
                        row += 1;
                    }
                }
                if row == 0 && !all.is_empty() {
                    lines.extend(wrapped(
                        &format!("No containers match \"{}\".", self.filter.as_str()),
                        Style::default(),
                    ));
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

    pub(super) fn footer(&self) -> String {
        if self.editing_filter {
            "type to filter  ←→ move  Enter apply  Esc cancel".into()
        } else if self.editing_user {
            "Enter apply  Esc cancel".into()
        } else {
            let clear_filter = if self.filter.is_empty() {
                ""
            } else {
                "  Esc clear filter"
            };
            format!(
                "↑↓ choose  / filter{clear_filter}  Enter shell  n new terminal  u user  r refresh  L this machine  Tab hosts"
            )
        }
    }

    fn state(&self, target: &Target) -> &PanelState {
        self.lists.get(target).unwrap_or(&PanelState::Unlisted)
    }

    fn listed(&self) -> Option<&ContainerList> {
        match self.state(&self.target()) {
            PanelState::Listed(list) => Some(list),
            _ => None,
        }
    }

    /// The containers of the current listing that match the filter, in the order they are
    /// shown, headers left out.
    fn rows(&self) -> Vec<&Container> {
        self.listed().map_or_else(Vec::new, |list| {
            let all = &list.listing.containers;
            group(all)
                .into_iter()
                .flat_map(|group| group.containers)
                .filter(|container| self.matches(container, all))
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

/// `text` wrapped at spaces to `width` columns, breaking a word longer than that. Continuation
/// lines keep the text's indentation unless it would take more than half the width.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if display_width(text) <= width {
        return vec![text.to_owned()];
    }
    let indent: String = text.chars().take_while(|&c| c == ' ').collect();
    let indent = if display_width(&indent) * 2 > width {
        String::new()
    } else {
        indent
    };
    let mut lines = Vec::new();
    let mut line = indent.clone();
    for mut word in text.split_whitespace() {
        loop {
            let empty = line.len() == indent.len();
            let gap = usize::from(!empty);
            if display_width(&line) + gap + display_width(word) <= width {
                if !empty {
                    line.push(' ');
                }
                line.push_str(word);
                break;
            }
            if !empty {
                lines.push(std::mem::replace(&mut line, indent.clone()));
                continue;
            }
            let room = width - display_width(&line);
            let mut split = 0;
            for (index, character) in word.char_indices() {
                let next = index + character.len_utf8();
                if split > 0 && display_width(&word[..next]) > room {
                    break;
                }
                split = next;
            }
            line.push_str(&word[..split]);
            lines.push(std::mem::replace(&mut line, indent.clone()));
            word = &word[split..];
            if word.is_empty() {
                break;
            }
        }
    }
    if line.len() > indent.len() {
        lines.push(line);
    }
    lines
}

/// Says on the terminal which command a fix is about to run, so it is on screen when sudo
/// asks for a password.
pub(super) fn show_fix(
    out: &mut impl std::io::Write,
    target: &Target,
    offer: &Offer,
) -> std::io::Result<()> {
    let command = remote_command(&offer.words());
    match target {
        Target::Local => writeln!(out, "This runs: {command}"),
        Target::Host(alias) => writeln!(out, "This runs on {alias}: {command}"),
    }?;
    out.flush()
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

    /// A panel showing `dev`, listed with [`containers`].
    fn listed_panel() -> Panel {
        let mut panel = Panel::default();
        panel.show(host("dev"));
        panel.finish(host("dev"), Ok(list(host("dev"), containers())));
        panel
    }

    fn type_filter(panel: &mut Panel, text: &str) {
        assert_eq!(press(panel, KeyCode::Char('/')), PanelAction::None);
        for character in text.chars() {
            assert_eq!(press(panel, KeyCode::Char(character)), PanelAction::None);
        }
        assert_eq!(press(panel, KeyCode::Enter), PanelAction::None);
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
    fn show_lists_once_per_target_and_again_after_a_failure() {
        let mut panel = Panel::default();
        assert_eq!(panel.show(host("dev")), PanelAction::List(host("dev")));
        panel.set_listing(host("dev"));
        assert_eq!(
            panel.show(host("dev")),
            PanelAction::None,
            "already listing"
        );
        panel.finish(host("dev"), Ok(list(host("dev"), containers())));
        assert_eq!(panel.show(host("dev")), PanelAction::None);

        assert_eq!(panel.show(host("web")), PanelAction::List(host("web")));
        panel.finish(host("web"), Err(issue(None)));
        assert_eq!(
            panel.show(host("web")),
            PanelAction::List(host("web")),
            "a failure is listed again"
        );
        assert_eq!(
            panel.show(host("dev")),
            PanelAction::None,
            "kept for the session"
        );
    }

    #[test]
    fn starts_on_this_machine_and_l_comes_back_to_it() {
        let mut panel = Panel::default();
        assert_eq!(panel.target(), Target::Local);
        assert_eq!(panel.title().to_string(), "Containers: this machine /");
        panel.show(host("dev"));
        assert_eq!(panel.target(), host("dev"));
        assert_eq!(panel.title().to_string(), "Containers: dev /");

        assert_eq!(
            press(&mut panel, KeyCode::Char('L')),
            PanelAction::List(Target::Local)
        );
        assert_eq!(panel.target(), Target::Local);
        panel.finish(Target::Local, Ok(list(Target::Local, containers())));
        assert_eq!(press(&mut panel, KeyCode::Char('L')), PanelAction::None);
    }

    #[test]
    fn selection_skips_headers() {
        let mut panel = listed_panel();
        let selected = |panel: &Panel| {
            texts(&panel.lines(true, 100, 200))
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
        panel.show(host("web"));
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
        assert_eq!(
            texts(&panel.lines(true, 100, 200)).last().unwrap(),
            "user: root"
        );

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
            texts(&panel.lines(true, 100, 200)).last().unwrap(),
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
            texts(&panel.lines(true, 100, 200)).last().unwrap(),
            "user: image default"
        );
        assert_eq!(
            panel.footer(),
            "↑↓ choose  / filter  Enter shell  n new terminal  u user  r refresh  L this machine  Tab hosts"
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
        panel.show(host("dev"));
        panel.set_listing(host("dev"));
        assert_eq!(texts(&panel.lines(true, 100, 200))[0], "Listing…");
        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::CancelList);
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
            ["Not listed: r lists containers"]
        );
        assert_eq!(
            panel.show(host("dev")),
            PanelAction::List(host("dev")),
            "lists again"
        );
    }

    #[test]
    fn offer_keys_only_for_matching_offer() {
        let mut panel = Panel::default();
        panel.show(host("dev"));
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
        panel.show(host("dev"));
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
            ["Not listed: r lists containers"]
        );

        let mut listed = list(host("dev"), containers());
        listed.via = Some(GroupTool::Sg);
        listed.note = Some("Listed through sg: other shells still lack the docker group.".into());
        listed.listing.skipped = 2;
        panel.finish(host("dev"), Ok(listed));
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
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
            texts(&panel.lines(true, 100, 200)).last().unwrap(),
            "user: image default"
        );

        panel.finish(host("dev"), Ok(list(host("dev"), Vec::new())));
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
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
        panel.show(host("dev"));
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
            texts(&panel.lines(false, 100, 200)),
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
            texts(&panel.lines(false, 100, 200)),
            ["ssh to dev failed", "ssh: Could not resolve hostname dev"]
        );

        panel.finish(host("dev"), Err(issue(Some(Offer::StartDaemon))));
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
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
            texts(&panel.lines(false, 100, 200)).last().unwrap(),
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
            texts(&panel.lines(false, 100, 200)),
            ["no container matches web on dev"]
        );
    }

    #[test]
    fn esc_during_refresh_restores_listing() {
        let mut panel = listed_panel();
        let before = texts(&panel.lines(false, 100, 200));
        assert_eq!(
            press(&mut panel, KeyCode::Char('r')),
            PanelAction::List(host("dev"))
        );
        panel.set_listing(host("dev"));
        assert_eq!(texts(&panel.lines(false, 100, 200)), ["Listing…"]);
        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::CancelList);
        assert_eq!(texts(&panel.lines(false, 100, 200)), before);
        assert_eq!(panel.show(host("dev")), PanelAction::None);

        panel.set_listing(host("dev"));
        panel.finish(host("dev"), Ok(list(host("dev"), Vec::new())));
        panel.set_listing(host("dev"));
        panel.finish(host("dev"), Err(issue(None)));
        panel.set_listing(host("dev"));
        press(&mut panel, KeyCode::Esc);
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
            ["Not listed: r lists containers"],
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
            texts(&panel.lines(true, 100, 200)).last().unwrap(),
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
        let shown = |panel: &Panel| texts(&panel.lines(true, 4, 200));
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
        assert_eq!(
            texts(&panel.lines(false, 2, 200)),
            ["> web  Up 2 hours", "wiki"]
        );
        assert_eq!(texts(&panel.lines(true, 0, 200)), ["user: image default"]);
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
        panel.show(host("web"));
        panel.set_listing(host("web"));
        panel.abandon(host("dev"));
        panel.abandon(host("web"));
        assert_eq!(panel.show(host("web")), PanelAction::List(host("web")));
        assert_eq!(panel.show(host("dev")), PanelAction::None);
        assert_eq!(texts(&panel.lines(false, 100, 200))[1], "> api  Up 2 hours");
    }

    #[test]
    fn long_failures_wrap_to_the_width() {
        assert_eq!(wrap("short", 10), ["short"]);
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        assert_eq!(
            wrap("  next: abcdefghij", 8),
            ["  next:", "  abcdef", "  ghij"]
        );
        assert_eq!(wrap("abc", 1), ["a", "b", "c"]);

        let join = Offer::JoinGroup {
            user: "deploy".into(),
            socket: "/var/run/docker.sock".into(),
        };
        let command = remote_command(&join.words());
        let mut panel = Panel::default();
        panel.show(host("dev"));
        let ContainerError::Docker(mut issue) = issue(Some(join)) else {
            unreachable!()
        };
        issue.next = vec![format!("Add deploy to the docker group: {command}")];
        panel.finish(host("dev"), Err(ContainerError::Docker(issue)));
        let lines = texts(&panel.lines(false, 100, 20));
        assert!(
            lines.iter().all(|line| line.chars().count() <= 20),
            "{lines:#?}"
        );
        let squashed = |text: &str| text.split_whitespace().collect::<String>();
        assert!(
            squashed(&lines.concat()).contains(&squashed(&command)),
            "{lines:#?}"
        );
        assert_eq!(lines[lines.len() - 2..], ["g add to docker", "group"]);
    }

    #[test]
    fn a_fix_shows_its_command_first() {
        let mut out = Vec::new();
        show_fix(&mut out, &host("dev"), &Offer::StartDaemon).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "This runs on dev: {}\n",
                remote_command(&Offer::StartDaemon.words())
            )
        );
        let mut out = Vec::new();
        show_fix(&mut out, &Target::Local, &Offer::StartDaemon).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .starts_with("This runs: sudo")
        );
    }

    #[test]
    fn filter_matches_shown_name_project_service_and_image() {
        let mut all = containers();
        all[3].image = "Redis:7".into();
        all.push(container("eeee", "Café-Bar", None, None));
        let shown = |filter: &str| {
            let mut panel = Panel::default();
            panel.finish(Target::Local, Ok(list(Target::Local, all.clone())));
            type_filter(&mut panel, filter);
            texts(&panel.lines(false, 100, 200))
        };
        assert_eq!(
            shown("API"),
            ["shop", "> api  Up 2 hours"],
            "shown name, any case"
        );
        assert_eq!(
            shown("wiki-db"),
            ["wiki", "> db  Up 2 hours"],
            "container name"
        );
        assert_eq!(
            shown("shop"),
            ["shop", "> api  Up 2 hours", "  web  Up 2 hours"],
            "project"
        );
        assert_eq!(shown("we"), ["shop", "> web  Up 2 hours"], "service");
        assert_eq!(shown("redis:7"), ["Other", "> redis  Up 2 hours"], "image");
        assert_eq!(
            shown("CAFE"),
            ["Other", "> Café-Bar  Up 2 hours"],
            "accents"
        );
        assert_eq!(shown("nope"), ["No containers match \"nope\"."]);
    }

    #[test]
    fn containers_filter_is_fuzzy() {
        let mut all = containers();
        all[3].image = "redis:7".into();
        let mut panel = Panel::default();
        panel.finish(Target::Local, Ok(list(Target::Local, all)));
        type_filter(&mut panel, "rds7");
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
            ["Other", "> redis  Up 2 hours"]
        );
    }

    #[test]
    fn containers_filter_highlights_the_shown_name() {
        let mut panel = listed_panel();
        type_filter(&mut panel, "rds");
        let lines = panel.lines(false, 100, 200);
        let row = lines
            .iter()
            .find(|line| line.to_string().contains("redis"))
            .unwrap();
        let hits: Vec<&str> = row
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(hits, ["r", "d", "s"]);
    }

    #[test]
    fn filter_keeps_the_selection_or_moves_to_the_first_match() {
        let mut panel = listed_panel();
        press(&mut panel, KeyCode::Down);
        type_filter(&mut panel, "w");
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
            ["shop", "> web  Up 2 hours", "wiki", "  db  Up 2 hours"],
            "web still matches"
        );
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Down);
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            PanelAction::Shell { id, .. } if id == "cccc"
        ));
        assert!(panel.details().unwrap().starts_with("name: wiki-db-1"));

        type_filter(&mut panel, "eb");
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
            ["shop", "> web  Up 2 hours"],
            "db no longer matches"
        );
        type_filter(&mut panel, "x");
        assert_eq!(press(&mut panel, KeyCode::Enter), PanelAction::None);
        assert_eq!(panel.details(), None);
    }

    #[test]
    fn filter_edit_esc_restores_and_esc_clears() {
        let mut panel = listed_panel();
        type_filter(&mut panel, "db");
        assert_eq!(panel.title().to_string(), "Containers: dev /db");
        assert_eq!(
            panel.footer(),
            "↑↓ choose  / filter  Esc clear filter  Enter shell  n new terminal  u user  r refresh  L this machine  Tab hosts"
        );

        press(&mut panel, KeyCode::Char('/'));
        assert_eq!(
            panel.footer(),
            "type to filter  ←→ move  Enter apply  Esc cancel"
        );
        press(&mut panel, KeyCode::Backspace);
        press(&mut panel, KeyCode::Backspace);
        assert_eq!(press(&mut panel, KeyCode::Char('q')), PanelAction::None);
        panel.paste("uu\n");
        assert_eq!(panel.title().to_string(), "Containers: dev /quu ");
        assert_eq!(
            texts(&panel.lines(false, 100, 200)),
            ["No containers match \"quu\"."]
        );
        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::None);
        assert_eq!(panel.title().to_string(), "Containers: dev /db");
        assert_eq!(texts(&panel.lines(false, 100, 200)).len(), 2);

        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::None, "clears");
        assert_eq!(panel.title().to_string(), "Containers: dev /");
        assert_eq!(texts(&panel.lines(false, 100, 200)).len(), 7);
        assert_eq!(press(&mut panel, KeyCode::Esc), PanelAction::Leave);
    }

    #[test]
    fn filtered_listing_scrolls_to_selection() {
        let mut panel = listed_panel();
        type_filter(&mut panel, "-");
        let shown = |panel: &Panel| texts(&panel.lines(true, 3, 200));
        assert_eq!(
            shown(&panel),
            ["shop", "> api  Up 2 hours", "user: image default"]
        );
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Down);
        assert_eq!(
            shown(&panel),
            ["wiki", "> db  Up 2 hours", "user: image default"],
            "redis is filtered out, so the selection stops at db"
        );
        type_filter(&mut panel, "x");
        type_filter(&mut panel, "");
        assert_eq!(
            shown(&panel),
            ["No containers match \"-x\".", "user: image default"],
            "a scroll past the end is clamped"
        );
    }

    #[test]
    fn another_target_clears_the_filter() {
        let mut panel = listed_panel();
        press(&mut panel, KeyCode::Down);
        type_filter(&mut panel, "web");
        panel.show(host("dev"));
        assert_eq!(
            panel.title().to_string(),
            "Containers: dev /web",
            "same target"
        );
        panel.show(Target::Local);
        assert_eq!(panel.title().to_string(), "Containers: this machine /");
        panel.finish(Target::Local, Ok(list(Target::Local, containers())));
        assert_eq!(texts(&panel.lines(false, 100, 200))[1], "> api  Up 2 hours");
    }
}
