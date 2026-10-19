//! `portico desktop install | uninstall | status`: plans, questions and reports.

use std::{
    io::{BufRead, Write},
    path::PathBuf,
    process::ExitCode,
};

use crate::desktop::{
    self, BindingPlan, DesktopEnv, Link, Step, Target,
    combo::Combo,
    keybinding::{self, Unavailable},
    launcher, panel,
};

#[derive(clap::Subcommand, Debug)]
pub enum DesktopCommand {
    /// Install the desktop panel, a launcher entry and a keybinding, each after a preview and
    /// a confirmation.
    Install {
        /// Install the panel as an Omarchy plugin.
        #[arg(long, conflicts_with = "quickshell")]
        omarchy: bool,
        /// Install the panel as a plain Quickshell configuration.
        #[arg(long)]
        quickshell: bool,
        /// Propose this key combination instead of SUPER + ALT + P.
        #[arg(long, value_name = "COMBO", conflicts_with = "no_bind")]
        bind: Option<String>,
        /// Leave out the keybinding.
        #[arg(long)]
        no_bind: bool,
        /// Leave out the panel (and the keybinding, which needs it).
        #[arg(long)]
        no_panel: bool,
        /// Leave out the launcher entry.
        #[arg(long)]
        no_launcher: bool,
        /// Accept every proposal without asking; a taken combination is still refused.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Remove what `portico desktop install` created, after a preview.
    Uninstall {
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Show what is installed.
    Status,
}

type Out<'a> = &'a mut dyn Write;

fn w(error: std::io::Error) -> String {
    error.to_string()
}

pub fn run(
    command: DesktopCommand,
    env: &DesktopEnv,
    input: &mut dyn BufRead,
    output: Out,
    interactive: bool,
) -> Result<ExitCode, String> {
    match command {
        DesktopCommand::Install {
            omarchy,
            quickshell,
            bind,
            no_bind,
            no_panel,
            no_launcher,
            yes,
        } => {
            let forced = match (omarchy, quickshell) {
                (true, _) => Some(Target::Omarchy),
                (_, true) => Some(Target::Quickshell),
                _ => None,
            };
            let combo = bind
                .as_deref()
                .map(Combo::parse)
                .transpose()?
                .unwrap_or_else(Combo::default_proposal);
            install(
                env,
                &Options {
                    forced,
                    combo,
                    bind: !no_bind,
                    panel: !no_panel,
                    launcher: !no_launcher,
                    yes,
                },
                input,
                output,
                interactive,
            )
        }
        DesktopCommand::Uninstall { yes } => uninstall(env, yes, input, output, interactive),
        DesktopCommand::Status => status(env, output).map(|()| ExitCode::SUCCESS),
    }
}

struct Options {
    forced: Option<Target>,
    combo: Combo,
    bind: bool,
    panel: bool,
    launcher: bool,
    yes: bool,
}

/// One component's part of the plan.
enum Part {
    /// Steps to confirm and apply.
    Pending(Vec<Step>),
    /// Nothing to do: why (`installed, 0.6.0`).
    Done(String),
    /// Cannot be installed: why.
    Unavailable(String),
    Skipped,
}

fn install(
    env: &DesktopEnv,
    options: &Options,
    input: &mut dyn BufRead,
    output: Out,
    interactive: bool,
) -> Result<ExitCode, String> {
    let version = env!("CARGO_PKG_VERSION");
    let target = if options.panel {
        match desktop::detect(env, options.forced) {
            Ok(found) => Some(found),
            Err(message) => {
                writeln!(output, "{message}").map_err(w)?;
                return Ok(ExitCode::FAILURE);
            }
        }
    } else {
        None
    };
    match &target {
        Some((target, why)) => writeln!(
            output,
            "Portico {version}: desktop integration for {} ({why}).",
            target.name()
        ),
        None => writeln!(
            output,
            "Portico {version}: desktop integration (launcher entry only)."
        ),
    }
    .map_err(w)?;
    let target = target.map(|(target, _)| target);

    let panel_part = match target {
        Some(target) => plan_panel(env, target),
        None => Part::Skipped,
    };
    let (launcher_part, launcher_notes) = if options.launcher {
        plan_launcher(env)
    } else {
        (Part::Skipped, Vec::new())
    };
    let panel_present = target.is_some_and(|target| {
        panel::state(&env.panel_dir()) != panel::State::Missing
            && desktop::link_state(&target.link(env), &env.panel_dir()) == Link::Ours
    });
    let mut binding = match target {
        Some(target) if options.bind => match &panel_part {
            Part::Unavailable(_) => {
                Binding::Unavailable("it toggles the panel, which cannot be installed".into())
            }
            _ => Binding::from(
                env,
                target,
                desktop::plan_binding(env, target, &options.combo),
            ),
        },
        _ => Binding::Skipped,
    };

    writeln!(output).map_err(w)?;
    print_part(env, output, "Panel", &panel_part, &[])?;
    print_part(
        env,
        output,
        "Launcher entry",
        &launcher_part,
        &launcher_notes,
    )?;
    if let Some(target) = target {
        print_binding(env, output, &binding, target)?;
    }

    let pending = matches!(panel_part, Part::Pending(_))
        || matches!(launcher_part, Part::Pending(_))
        || matches!(binding, Binding::Ready(..) | Binding::Taken { .. });
    if !pending {
        writeln!(output, "\nNothing to install.").map_err(w)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Binding::Taken { combo, by } = &binding
        && options.yes
    {
        return Err(format!(
            "{combo} is taken ({by}). Choose another with --bind, or skip the keybinding with --no-bind."
        ));
    }
    if !options.yes && !interactive {
        return Err("desktop install requires --yes in non-interactive mode".into());
    }

    let ask = |input: &mut dyn BufRead, output: Out, question: &str| -> Result<bool, String> {
        if options.yes {
            return Ok(true);
        }
        let answer = prompt(input, output, &format!("{question} [y/N] "))?;
        Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
    };
    let launcher_pending = matches!(launcher_part, Part::Pending(_));
    let panel_pending = matches!(panel_part, Part::Pending(_));
    let panel_steps = match panel_part {
        Part::Pending(steps) if ask(input, output, "Install the panel?")? => Some(steps),
        _ => None,
    };
    let launcher_steps = match launcher_part {
        Part::Pending(steps) if ask(input, output, "Install the launcher entry?")? => Some(steps),
        _ => None,
    };
    let panel_ready = panel_present || panel_steps.is_some();
    if matches!(binding, Binding::Ready(..) | Binding::Taken { .. }) && !panel_ready {
        binding = Binding::Unavailable("it toggles the panel, which is not installed".into());
    }
    let binding_steps = match target {
        Some(target) if !options.yes => {
            confirm_binding(env, target, binding.clone(), input, output)?
        }
        _ => match &binding {
            Binding::Ready(plan) => Some(plan.clone()),
            _ => None,
        },
    };

    if !options.yes {
        writeln!(output).map_err(w)?;
    }
    let mut failed = false;
    if let Some(steps) = &panel_steps {
        failed |= !apply_all(env, output, steps)?;
    } else if panel_pending {
        writeln!(output, "Skipped the panel.").map_err(w)?;
    }
    if let Some(steps) = &launcher_steps {
        failed |= !apply_all(env, output, steps)?;
    } else if launcher_pending {
        writeln!(output, "Skipped the launcher entry.").map_err(w)?;
    }
    let mut bound: Option<Combo> = None;
    if let Some(ready) = &binding_steps {
        if apply_all(env, output, std::slice::from_ref(&ready.step))? {
            bound = Some(ready.combo.clone());
            if ready.live && !desktop::hyprland_has_binding(env, &ready.combo) {
                writeln!(
                    output,
                    "Hyprland has not picked up {} yet; if it does not work, run hyprctl reload.",
                    ready.combo
                )
                .map_err(w)?;
            }
            if !ready.live {
                match env.run("swaymsg", &["reload"]) {
                    Ok(_) => writeln!(output, "Ran swaymsg reload.").map_err(w)?,
                    Err(error) => {
                        failed = true;
                        writeln!(
                            output,
                            "error: {error}\n  Reload Sway to use the keybinding."
                        )
                        .map_err(w)?;
                    }
                }
            }
        } else {
            failed = true;
        }
    }

    writeln!(output).map_err(w)?;
    if failed {
        writeln!(
            output,
            "Some steps failed (see above). Fix them and run portico desktop install again."
        )
        .map_err(w)?;
    } else {
        writeln!(output, "Done.").map_err(w)?;
    }
    if let Some(target) = target
        && (panel_present || panel_steps.is_some())
    {
        let key = bound
            .map(|combo| format!(" with {combo}"))
            .unwrap_or_default();
        match target {
            Target::Omarchy => {
                writeln!(
                    output,
                    "Open the panel from the Portico icon in the bar{}.",
                    key.replacen(" with", " or with", 1)
                )
            }
            Target::Quickshell => if key.is_empty() {
                writeln!(
                    output,
                    "Toggle the panel with: {}",
                    keybinding::QUICKSHELL_TOGGLE
                )
            } else {
                writeln!(output, "Toggle the panel{key}.")
            }
            .and_then(|()| writeln!(output, "{}", keybinding::autostart_hint(env))),
        }
        .map_err(w)?;
        if let Binding::Other = binding {
            writeln!(
                output,
                "Portico cannot add a keybinding for this compositor. Bind this command to a key: {}",
                keybinding::toggle_command(target)
            )
            .map_err(w)?;
        }
    }
    writeln!(
        output,
        "Run portico desktop status to check it, and portico desktop uninstall to remove it."
    )
    .map_err(w)?;
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn plan_panel(env: &DesktopEnv, target: Target) -> Part {
    let dir = env.panel_dir();
    let link = target.link(env);
    let mut steps = Vec::new();
    let state = panel::state(&dir);
    if state != panel::State::Current {
        steps.push(Step::WritePanel);
    }
    let link_state = desktop::link_state(&link, &dir);
    match &link_state {
        Link::Blocked => {
            return Part::Unavailable(format!(
                "{} exists and is not a link; move it away to install the panel",
                env.show(&link)
            ));
        }
        Link::Ours => {}
        Link::Missing => steps.push(Step::Link {
            path: link.clone(),
            replaces: None,
        }),
        Link::Elsewhere(old) => steps.push(Step::Link {
            path: link.clone(),
            replaces: Some(old.clone()),
        }),
    }
    let run = |args: &[&str]| Step::Run(args.iter().map(|arg| arg.to_string()).collect());
    match target {
        Target::Omarchy => {
            let enabled = desktop::omarchy_enabled(env);
            if link_state != Link::Ours || enabled != Some(true) {
                steps.push(run(&["omarchy-shell", "shell", "rescanPlugins"]));
            }
            if enabled != Some(true) {
                steps.push(run(&["omarchy", "plugin", "enable", "portico"]));
            }
        }
        Target::Quickshell => {
            if link_state != Link::Ours || !desktop::quickshell_running(env) {
                steps.push(run(&["qs", "-c", "portico", "-n", "-d"]));
            }
        }
    }
    if steps.is_empty() {
        Part::Done(format!(
            "installed, {} ({} -> {})",
            env!("CARGO_PKG_VERSION"),
            env.show(&link),
            env.show(&dir)
        ))
    } else {
        Part::Pending(steps)
    }
}

fn plan_launcher(env: &DesktopEnv) -> (Part, Vec<String>) {
    let (desktop_path, icon_path) = launcher::paths(env);
    for path in [&desktop_path, &icon_path] {
        if path.exists() && !launcher::is_marked(path) {
            return (
                Part::Unavailable(format!(
                    "{} exists and is not Portico's; move it away to install the launcher entry",
                    env.show(path)
                )),
                Vec::new(),
            );
        }
    }
    if !desktop_path.exists()
        && let Some(system) = launcher::system_entry(env)
    {
        return (
            Part::Done(format!("provided by {}", system.display())),
            Vec::new(),
        );
    }
    let exe = env.exe.clone().unwrap_or_else(|| PathBuf::from("portico"));
    let mut notes = Vec::new();
    if env.is_development_build() {
        notes.push(format!(
            "{} is a cargo build directory's binary: the launcher entry would run it",
            env.show(&exe)
        ));
    }
    let mut steps = Vec::new();
    for (path, contents) in [
        (desktop_path, launcher::desktop_file(&exe)),
        (icon_path, launcher::icon_file()),
    ] {
        if std::fs::read_to_string(&path).ok().as_deref() != Some(contents.as_str()) {
            steps.push(Step::WriteFile { path, contents });
        }
    }
    if steps.is_empty() {
        (
            Part::Done(format!("installed (runs {})", env.show(&exe))),
            notes,
        )
    } else {
        (Part::Pending(steps), notes)
    }
}

/// The keybinding as the plan shows it and the questions handle it.
#[derive(Clone)]
enum Binding {
    Ready(Ready),
    Bound(String),
    Taken {
        combo: Combo,
        by: String,
    },
    Unavailable(String),
    /// Another compositor: the command is printed.
    Other,
    Skipped,
}

#[derive(Clone)]
struct Ready {
    combo: Combo,
    step: Step,
    live: bool,
}

impl Binding {
    fn from(env: &DesktopEnv, _target: Target, plan: BindingPlan) -> Self {
        match plan {
            BindingPlan::Ready {
                combo, step, live, ..
            } => Binding::Ready(Ready { combo, step, live }),
            BindingPlan::Bound { location, line } => {
                Binding::Bound(format!("in {}, line {line}", env.show(&location.file)))
            }
            BindingPlan::Taken { combo, by } => Binding::Taken { combo, by },
            BindingPlan::Unavailable(Unavailable::OtherCompositor) => Binding::Other,
            BindingPlan::Unavailable(Unavailable::Reason(reason)) => Binding::Unavailable(reason),
        }
    }
}

fn print_part(
    env: &DesktopEnv,
    output: Out,
    title: &str,
    part: &Part,
    notes: &[String],
) -> Result<(), String> {
    match part {
        Part::Skipped => Ok(()),
        Part::Done(why) => writeln!(output, "{title}: {why}\n").map_err(w),
        Part::Unavailable(why) => writeln!(output, "{title}: unavailable, {why}\n").map_err(w),
        Part::Pending(steps) => {
            writeln!(output, "{title}").map_err(w)?;
            for step in steps {
                print_step(env, output, step)?;
            }
            for note in notes {
                writeln!(output, "  note     {note}").map_err(w)?;
            }
            writeln!(output).map_err(w)
        }
    }
}

fn print_step(env: &DesktopEnv, output: Out, step: &Step) -> Result<(), String> {
    let show = |path: &std::path::Path| env.show(path);
    match step {
        Step::WritePanel => writeln!(
            output,
            "  write    {}/ ({} files and VERSION, {})",
            show(&env.panel_dir()),
            panel::FILES.len(),
            env!("CARGO_PKG_VERSION")
        ),
        Step::Link {
            path,
            replaces: None,
        } => writeln!(
            output,
            "  link     {} -> {}",
            show(path),
            show(&env.panel_dir())
        ),
        Step::Link {
            path,
            replaces: Some(old),
        } => writeln!(
            output,
            "  replace  {}, a link to {},\n           with a link to {}",
            show(path),
            show(old),
            show(&env.panel_dir())
        ),
        Step::Run(args) => {
            let line = args.join(" ");
            if args.first().is_some_and(|program| program == "qs") && args.iter().any(|a| a == "-d")
            {
                writeln!(
                    output,
                    "  run      {line}   (starts the panel now, hidden until toggled)"
                )
            } else {
                writeln!(output, "  run      {line}")
            }
        }
        Step::WriteFile { path, contents } => match launcher::exec_of_text(contents) {
            Some(exec) if path.extension().is_some_and(|ext| ext == "desktop") => {
                writeln!(output, "  write    {} (runs {exec})", show(path))
            }
            _ => writeln!(output, "  write    {}", show(path)),
        },
        Step::Edit {
            file,
            backup,
            action,
            lines,
            ..
        } => writeln!(output, "  backup   {} to {}", show(file), show(backup))
            .and_then(|()| writeln!(output, "  {action}:"))
            .and_then(|()| {
                lines
                    .iter()
                    .try_for_each(|line| writeln!(output, "             {line}"))
            }),
        Step::Remove(path) => writeln!(output, "  remove   {}", show(path)),
        Step::RemovePanel => writeln!(output, "  remove   {}/", show(&env.panel_dir())),
    }
    .map_err(w)
}

fn print_binding(
    env: &DesktopEnv,
    output: Out,
    binding: &Binding,
    target: Target,
) -> Result<(), String> {
    match binding {
        Binding::Skipped => Ok(()),
        Binding::Bound(place) => writeln!(output, "Keybinding: already bound {place}\n").map_err(w),
        Binding::Unavailable(why) => writeln!(output, "Keybinding: unavailable, {why}\n").map_err(w),
        Binding::Other => writeln!(
            output,
            "Keybinding: Portico can bind keys in Hyprland and Sway only; bind this command yourself:\n  {}\n",
            keybinding::toggle_command(target)
        )
        .map_err(w),
        Binding::Taken { combo, by } => {
            writeln!(output, "Keybinding: {combo} is taken ({by})\n").map_err(w)
        }
        Binding::Ready(ready) => {
            let checked = if ready.live {
                "checked with hyprctl binds"
            } else {
                "checked in the Sway configuration"
            };
            writeln!(output, "Keybinding: {}, free ({checked})", ready.combo).map_err(w)?;
            if let Step::Edit { file, backup, .. } = &ready.step {
                writeln!(output, "  backup   {} to {}", env.show(file), env.show(backup))
                    .map_err(w)?;
            }
            print_edit(output, &ready.step)?;
            writeln!(output).map_err(w)
        }
    }
}

fn print_edit(output: Out, step: &Step) -> Result<(), String> {
    if let Step::Edit { action, lines, .. } = step {
        writeln!(output, "  {action}:").map_err(w)?;
        for line in lines {
            writeln!(output, "             {line}").map_err(w)?;
        }
    }
    Ok(())
}

/// Asks about the keybinding, accepting another combination; the edit to make, if any.
fn confirm_binding(
    env: &DesktopEnv,
    target: Target,
    mut binding: Binding,
    input: &mut dyn BufRead,
    output: Out,
) -> Result<Option<Ready>, String> {
    loop {
        let answer = match &binding {
            Binding::Ready(ready) => prompt(
                input,
                output,
                &format!(
                    "Add the keybinding {}? Type y, n, or another combination [y/N] ",
                    ready.combo
                ),
            )?,
            Binding::Taken { combo, by } => prompt(
                input,
                output,
                &format!(
                    "{combo} is taken ({by}). Type another combination, or press Enter to skip: "
                ),
            )?,
            _ => return Ok(None),
        };
        let lower = answer.to_ascii_lowercase();
        if let Binding::Ready(ready) = &binding
            && matches!(lower.as_str(), "y" | "yes")
        {
            return Ok(Some(ready.clone()));
        }
        if matches!(lower.as_str(), "" | "n" | "no") {
            return Ok(None);
        }
        let combo = match Combo::parse(&answer) {
            Ok(combo) => combo,
            Err(problem) => {
                writeln!(output, "{problem}.").map_err(w)?;
                continue;
            }
        };
        binding = Binding::from(env, target, desktop::plan_binding(env, target, &combo));
        match &binding {
            Binding::Ready(ready) => print_edit(output, &ready.step)?,
            Binding::Bound(place) => {
                writeln!(output, "{combo} is already bound {place}.").map_err(w)?;
                return Ok(None);
            }
            Binding::Taken { .. } => {}
            other => {
                print_binding(env, output, other, target)?;
                return Ok(None);
            }
        }
    }
}

fn prompt(input: &mut dyn BufRead, output: Out, question: &str) -> Result<String, String> {
    write!(output, "{question}").map_err(w)?;
    output.flush().map_err(w)?;
    let mut answer = String::new();
    if input.read_line(&mut answer).map_err(w)? == 0 {
        // End of input: stop before any change.
        writeln!(output).map_err(w)?;
        return Err("no answer; nothing was changed".into());
    }
    Ok(answer.trim().to_string())
}

/// Applies `steps` in order, reporting each; false when one failed (the rest still run, since
/// each is independent and idempotent).
fn apply_all(env: &DesktopEnv, output: Out, steps: &[Step]) -> Result<bool, String> {
    let mut ok = true;
    for step in steps {
        match desktop::apply(env, step) {
            Ok(said) => report(env, output, step, said)?,
            Err(error) => {
                ok = false;
                writeln!(output, "error: {error}").map_err(w)?;
            }
        }
    }
    Ok(ok)
}

fn report(env: &DesktopEnv, output: Out, step: &Step, said: Option<String>) -> Result<(), String> {
    let show = |path: &std::path::Path| env.show(path);
    match step {
        Step::WritePanel => writeln!(
            output,
            "Wrote {} ({}).",
            show(&env.panel_dir()),
            env!("CARGO_PKG_VERSION")
        ),
        Step::Link { path, .. } => writeln!(
            output,
            "Linked {} -> {}.",
            show(path),
            show(&env.panel_dir())
        ),
        Step::Run(args) => match said {
            Some(said) => writeln!(output, "Ran {}: {said}", args.join(" ")),
            None => writeln!(output, "Ran {}.", args.join(" ")),
        },
        Step::WriteFile { path, .. } => writeln!(output, "Wrote {}.", show(path)),
        Step::Edit {
            file,
            backup,
            action,
            ..
        } => {
            let verb = if action.starts_with("remove") {
                "Removed Portico's keybinding from"
            } else {
                "Added the keybinding to"
            };
            writeln!(output, "{verb} {} (backup {}).", show(file), show(backup))
        }
        Step::Remove(path) => writeln!(output, "Removed {}.", show(path)),
        Step::RemovePanel => writeln!(output, "Removed {}.", show(&env.panel_dir())),
    }
    .map_err(w)
}

fn uninstall(
    env: &DesktopEnv,
    yes: bool,
    input: &mut dyn BufRead,
    output: Out,
    interactive: bool,
) -> Result<ExitCode, String> {
    let dir = env.panel_dir();
    let run = |args: &[&str]| Step::Run(args.iter().map(|arg| arg.to_string()).collect());
    let mut steps = Vec::new();
    let mut problems = Vec::new();

    let omarchy_link = Target::Omarchy.link(env);
    if desktop::link_state(&omarchy_link, &dir) == Link::Ours {
        if env.find("omarchy").is_some() && desktop::omarchy_enabled(env) == Some(true) {
            steps.push(run(&["omarchy", "plugin", "disable", "portico"]));
        }
        steps.push(Step::Remove(omarchy_link));
        if env.find("omarchy-shell").is_some() {
            steps.push(run(&["omarchy-shell", "shell", "rescanPlugins"]));
        }
    }
    let quickshell_link = Target::Quickshell.link(env);
    if desktop::link_state(&quickshell_link, &dir) == Link::Ours {
        if desktop::quickshell_running(env) {
            steps.push(run(&["qs", "kill", "-c", "portico"]));
        }
        steps.push(Step::Remove(quickshell_link));
    }
    let mut sway = false;
    for (setup, file, block) in desktop::installed_bindings(env) {
        match block {
            Ok(block) => {
                let Ok(original) = std::fs::read_to_string(&file) else {
                    continue;
                };
                let lines: Vec<String> = original
                    .lines()
                    .skip(block.first)
                    .take(block.last - block.first + 1)
                    .map(str::to_string)
                    .collect();
                sway |= setup == keybinding::Setup::Sway;
                steps.push(Step::Edit {
                    text: keybinding::replace_block(&original, &block, None),
                    backup: desktop::backup_path(&file),
                    action: format!(
                        "remove   lines {}-{} of {}",
                        block.first + 1,
                        block.last + 1,
                        env.show(&file)
                    ),
                    file,
                    original,
                    lines,
                });
            }
            Err(problem) => problems.push(format!(
                "{} cannot be edited safely: {problem}; remove Portico's lines by hand",
                env.show(&file)
            )),
        }
    }
    if sway && env.sway {
        steps.push(run(&["swaymsg", "reload"]));
    }
    let (desktop_path, icon_path) = launcher::paths(env);
    for path in [desktop_path, icon_path] {
        if launcher::is_marked(&path) {
            steps.push(Step::Remove(path));
        }
    }
    if dir.exists() {
        steps.push(Step::RemovePanel);
    }

    for problem in &problems {
        writeln!(output, "note: {problem}").map_err(w)?;
    }
    if steps.is_empty() {
        writeln!(output, "Portico's desktop integration is not installed.").map_err(w)?;
        return Ok(if problems.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    writeln!(output, "Portico's desktop integration:").map_err(w)?;
    for step in &steps {
        print_step(env, output, step)?;
    }
    if !yes {
        if !interactive {
            return Err("desktop uninstall requires --yes in non-interactive mode".into());
        }
        let answer = prompt(input, output, "Remove the desktop integration? [y/N] ")?;
        if !matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
            writeln!(output, "Nothing was removed.").map_err(w)?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    let ok = apply_all(env, output, &steps)?;
    Ok(if ok && problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn status(env: &DesktopEnv, output: Out) -> Result<(), String> {
    let dir = env.panel_dir();
    let version = env!("CARGO_PKG_VERSION");
    let panel = match panel::state(&dir) {
        panel::State::Missing => "not installed".to_string(),
        panel::State::Current => format!(
            "installed in {}, {version} (matches this portico)",
            env.show(&dir)
        ),
        panel::State::Stale(written) => format!(
            "installed in {}, written by {written}; this portico is {version}, run portico desktop install",
            env.show(&dir)
        ),
        panel::State::Modified => format!(
            "installed in {}, but its files differ from this portico's; run portico desktop install",
            env.show(&dir)
        ),
    };
    writeln!(output, "Panel        {panel}").map_err(w)?;
    for target in [Target::Omarchy, Target::Quickshell] {
        let link = target.link(env);
        let state = match desktop::link_state(&link, &dir) {
            Link::Missing => "not linked".to_string(),
            Link::Ours => {
                let mut text = format!("{} -> {}", env.show(&link), env.show(&dir));
                if target == Target::Omarchy {
                    text.push_str(match desktop::omarchy_enabled(env) {
                        Some(true) => ", enabled",
                        Some(false) => ", Omarchy plugin disabled",
                        None => ", enabled state unknown (omarchy could not be asked)",
                    });
                }
                text
            }
            Link::Elsewhere(other) => format!(
                "{} links to {}, not Portico's copy",
                env.show(&link),
                env.show(&other)
            ),
            Link::Blocked => format!("{} is not a link, not Portico's", env.show(&link)),
        };
        writeln!(output, "  {:<10} {state}", target.name()).map_err(w)?;
    }

    let (desktop_path, icon_path) = launcher::paths(env);
    let mut lines = Vec::new();
    if launcher::is_marked(&desktop_path) {
        let exec = launcher::exec_of(&desktop_path).unwrap_or_default();
        let runs = if exec.starts_with('/') && !std::path::Path::new(&exec).exists() {
            format!("runs {exec}, which no longer exists")
        } else {
            format!("runs {exec}")
        };
        lines.push(format!("{} ({runs})", env.show(&desktop_path)));
    } else if desktop_path.exists() {
        lines.push(format!(
            "{} is an unmarked file, not Portico's",
            env.show(&desktop_path)
        ));
    }
    if launcher::is_marked(&icon_path) {
        lines.push(env.show(&icon_path));
    }
    if lines.is_empty() {
        lines.push(match launcher::system_entry(env) {
            Some(system) => format!("provided by {}", system.display()),
            None => "not installed".into(),
        });
    }
    for (index, line) in lines.iter().enumerate() {
        let label = if index == 0 { "Launcher" } else { "" };
        writeln!(output, "{label:<12} {line}").map_err(w)?;
    }

    let bindings = desktop::installed_bindings(env);
    if bindings.is_empty() {
        writeln!(output, "Keybinding   not installed").map_err(w)?;
    }
    for (_, file, block) in bindings {
        match block {
            Ok(block) => writeln!(
                output,
                "Keybinding   {}, line {}: {}",
                env.show(&file),
                block.first + 2,
                block.line
            ),
            Err(problem) => writeln!(output, "Keybinding   {}: {problem}", env.show(&file)),
        }
        .map_err(w)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_executable::write_executable;
    use std::{ffi::OsString, fs, path::Path};

    const BINDINGS: &str = "-- mine\n";

    /// Omarchy on Hyprland in a temporary home, with fake programs; SUPER + P is taken.
    fn omarchy() -> (tempfile::TempDir, DesktopEnv) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("home");
        let bin = root.join("bin");
        fs::create_dir_all(home.join(".config/omarchy/plugins")).unwrap();
        fs::create_dir_all(home.join(".config/hypr")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(home.join(".config/hypr/bindings.lua"), BINDINGS).unwrap();
        write_executable(bin.join("omarchy-shell"), "#!/bin/sh\nexit 0\n");
        write_executable(
            bin.join("omarchy"),
            "#!/bin/sh\n[ \"$2\" = list ] && echo '[]'\n[ \"$2\" = enable ] && echo \"Enabled $3\"\nexit 0\n",
        );
        write_executable(
            bin.join("hyprctl"),
            "#!/bin/sh\necho '[{\"mouse\":false,\"modmask\":64,\"submap\":\"\",\"key\":\"P\",\"description\":\"Pseudo window\"}]'\n",
        );
        let path = OsString::from(&bin);
        let env = DesktopEnv::new(
            home,
            move |name| match name {
                "PATH" => Some(path.clone()),
                "HYPRLAND_INSTANCE_SIGNATURE" => Some("x".into()),
                _ => None,
            },
            Some("/usr/bin/portico".into()),
        );
        (dir, env)
    }

    fn install(env: &DesktopEnv, answers: &str) -> (Result<ExitCode, String>, String) {
        let mut output = Vec::new();
        let result = run(
            DesktopCommand::Install {
                omarchy: false,
                quickshell: false,
                bind: None,
                no_bind: false,
                no_panel: false,
                no_launcher: false,
                yes: false,
            },
            env,
            &mut answers.as_bytes(),
            &mut output,
            true,
        );
        (result, String::from_utf8(output).unwrap())
    }

    fn bindings(env: &DesktopEnv) -> String {
        fs::read_to_string(env.config_home.join("hypr/bindings.lua")).unwrap()
    }

    #[test]
    fn asks_per_component_and_accepts_another_combination() {
        let (_dir, env) = omarchy();
        let (result, out) = install(&env, "y\nn\nSUPER + P\nnot a combo +\nCTRL + ALT + P\ny\n");
        assert!(result.is_ok(), "{out}");
        assert!(out.contains("Install the panel? [y/N] Install the launcher entry? [y/N] "), "{out}");
        assert!(
            out.contains("Add the keybinding SUPER + ALT + P? Type y, n, or another combination [y/N] "),
            "{out}"
        );
        assert!(out.contains("SUPER + P is taken (Pseudo window). Type another combination, or press Enter to skip: "), "{out}");
        assert!(out.contains("\"not a combo +\" names more than one key."), "{out}");
        assert!(out.contains("o.bind(\"CTRL + ALT + P\", \"Portico\""), "{out}");
        assert!(out.contains("Skipped the launcher entry."), "{out}");
        assert!(env.panel_dir().join("VERSION").is_file());
        assert!(!launcher::paths(&env).0.exists());
        assert_eq!(
            bindings(&env),
            format!(
                "{BINDINGS}-- portico desktop install: begin\no.bind(\"CTRL + ALT + P\", \"Portico\", \"omarchy-shell shell toggle portico\")\n-- portico desktop install: end\n"
            )
        );
    }

    #[test]
    fn declining_the_panel_skips_the_keybinding() {
        let (_dir, env) = omarchy();
        let (result, out) = install(&env, "n\ny\n");
        assert!(result.is_ok(), "{out}");
        assert!(!out.contains("Add the keybinding"), "{out}");
        assert!(!env.panel_dir().exists());
        assert!(launcher::paths(&env).0.exists());
        assert_eq!(bindings(&env), BINDINGS);
    }

    #[test]
    fn stopping_at_a_question_changes_nothing() {
        let (_dir, env) = omarchy();
        let (result, out) = install(&env, "y\ny\n");
        assert_eq!(result.unwrap_err(), "no answer; nothing was changed", "{out}");
        assert!(!env.panel_dir().exists());
        assert!(!launcher::paths(&env).0.exists());
        assert_eq!(bindings(&env), BINDINGS);
        assert!(!Path::new(&env.config_home.join("omarchy/plugins/portico")).exists());
    }
}
