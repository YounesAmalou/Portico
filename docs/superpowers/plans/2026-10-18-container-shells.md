# Container Shells Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** List running Docker containers on this machine or on a known host, grouped by Compose project, and open a shell in one from the CLI (with a picker) and from a TUI containers panel.

**Architecture:** Pure logic (parsing, grouping, references, quoting, commands, problem classification, the group probe) lives in a new `src/container.rs`. `App` gains a `Target`, and it runs docker either directly or as an ssh remote command, using small additions to `src/openssh.rs` that reuse the connect and new-terminal paths. The CLI adds `containers` and `shell`. The TUI gets a containers panel in a new `src/tui/containers.rs`, with a listing worker modelled on `src/tui/tester.rs`, and only thin hooks in `src/tui.rs`.

**Tech Stack:** Rust 2024, clap 4, ratatui 0.30, crossterm 0.29. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-18-container-shells-design.md`

## Global Constraints

- No new crate dependencies. JSON lines are parsed by a hand-written flat-object parser.
- Only `docker` is run, never `podman`. Never `docker attach`.
- Nothing is written to disk by this feature (no lists, users or preferences).
- Every ssh invocation for containers passes `-o RemoteCommand=none`, and the TUI listing also passes `-o BatchMode=yes`.
- Every privileged fix runs through `sudo` in the user's terminal, shown in full before running. Portico never reads a password.
- Do not edit `ROADMAP.md`. Keep edits to `src/tui.rs`, `src/openssh.rs` and `src/app.rs` small: other branches touch them.
- Tests run as a regular user (never root). `scripts/ci-checks.sh fmt`, `clippy` and `test` must pass before each commit.
- Commits follow Conventional Commits, with no AI attribution trailers or lines. Branch: `feat/container-shells`.

## Review Focus

1. **Shell noise on stdout over ssh** (a `.bashrc` that echoes). Lines that do not start with `{` are ignored silently. Only lines starting with `{` that fail to parse count as skipped. (This refines the spec's "a line that does not parse is skipped and counted".) The test lives in Task 1.
2. **Quoting of hostile values in the remote command:** users like `o'brien` or `a b`, and an empty word. The test lives in Task 3.
3. **No running containers:** `containers` prints the empty message, and `shell` with no reference prints it and exits 1 without opening a picker. The test lives in Task 8.
4. **A shell that exits non-zero for its own reasons** (an exit of 1 or 130 after Ctrl+C) is passed through and never classified as a Docker problem. The test lives in Task 8.
5. **Alias case:** `--on DEV` for host `dev` uses the inventory's spelling in ssh's arguments, as `connect` does. The test lives in Task 7.

---

### Task 1: Container model, listing parser and grouping

**Files:**
- Create: `src/container.rs`
- Modify: `src/lib.rs` (add `pub mod container;`)

**Interfaces:**
- Produces:
  - `pub struct Container { pub id: String, pub name: String, pub image: String, pub command: String, pub running_for: String, pub status: String, pub ports: String, pub networks: String, pub mounts: String, pub project: Option<String>, pub service: Option<String>, pub number: Option<u32>, pub working_dir: Option<String> }`, deriving `Clone, Debug, Eq, PartialEq`. Empty labels become `None`.
  - `pub struct Listing { pub containers: Vec<Container>, pub skipped: usize }`
  - `pub const PS_FORMAT: &str`, a template printing one object with the keys `id,name,image,command,running_for,status,ports,networks,mounts,project,service,number,working_dir`. Each value is written as `{{json .Field}}`, and labels as `{{json (.Label "com.docker.compose.project")}}` and so on for `.service`, `.container-number` and `.project.working_dir`.
  - `pub fn ps_words() -> Vec<String>`, which returns `["docker","ps","--no-trunc","--format",PS_FORMAT]`
  - `pub fn parse_listing(stdout: &str) -> Listing`
  - `pub struct Group<'a> { pub project: Option<&'a str>, pub containers: Vec<&'a Container> }`
  - `pub fn group(containers: &[Container]) -> Vec<Group<'_>>`
  - `pub fn display_name(container: &Container, all: &[Container]) -> String`

- [x] **Step 1: Write the failing tests** in `src/container.rs` `mod tests`:
  - `parses_a_ps_line`: a line with every key yields the matching `Container`. `number: "2"` becomes `Some(2)`, and `project: ""` becomes `None`.
  - `parses_escapes`: values containing `\"`, `\\`, `\n`, `\t`, `é` and `…` decode to `"`, `\`, a newline, a tab, `é` and `…`.
  - `ignores_shell_noise_and_counts_broken_objects`: the input `"welcome!\n{\"id\":\"a\"...valid...}\n{\"id\": broken\n"` gives 1 container and `skipped == 1`. Also, `parse_listing("")` gives an empty listing with `skipped == 0`.
  - `groups_projects_then_other`: containers `web`/`shop#1`, `db`/`shop#1`, `api`/`alpha#1` and a plain `pgadmin` group as `[alpha:[api], shop:[db, web], Other:[pgadmin]]`, where `project` is `None` for Other.
  - `display_names_number_replicas_only`: `shop/web#1` and `shop/web#2` display `web#1` and `web#2`, the single `shop/db#1` displays `db`, and the plain container shows its name.
- [x] **Step 2: Run** `cargo test --lib container::` and expect a compile failure (module missing).
- [x] **Step 3: Implement.** The parser reads one object of string values: whitespace, `{`, then `"key"` `:` `"value"` pairs separated by `,`, then `}`. It returns `None` on anything else, and a missing key is an empty string. Sort within a project by `(service, number)`, and sort Other by name.
- [x] **Step 4: Run** `cargo test --lib container::` and expect PASS.
- [x] **Step 5: Commit** with `git commit -m "feat: parse and group running docker containers"`

### Task 2: Container references

**Files:** Modify `src/container.rs`

**Interfaces:**
- Produces:
  - `pub enum ResolveError { NotFound, Ambiguous(Vec<String>) }`, where `Ambiguous` holds the candidates' container names, sorted.
  - `pub fn resolve<'a>(reference: &str, containers: &'a [Container]) -> Result<&'a Container, ResolveError>`

- [x] **Step 1: Write the failing tests:**
  - `resolves_exact_name_first`: a container named `web` wins over a service called `web` in another container.
  - `resolves_project_service_and_number`: `shop/web` (single replica), and `shop/web#2` among two replicas.
  - `resolves_unique_service`: `db` when only `shop` has `db`.
  - `ambiguous_service_lists_names`: `web` in projects `shop` and `blog` gives `Ambiguous(["blog-web-1","shop-web-1"])`. `shop/web` with two replicas is also ambiguous.
  - `resolves_id_prefix_of_four_or_more_hex`: `3174` matches. `317` gives `NotFound`, and `zzzz` gives `NotFound`. A prefix matching two IDs is `Ambiguous`.
  - `unknown_reference_is_not_found`
- [x] **Step 2: Run** `cargo test --lib container::` and expect FAIL.
- [x] **Step 3: Implement** `resolve` with the order from the spec. Name, then `project/service[#n]`, then `service`, then ID prefix. The first form that matches anything decides: one match means `Ok`, several mean `Ambiguous`.
- [x] **Step 4: Run the tests** and expect PASS.
- [x] **Step 5: Commit** with `git commit -m "feat: resolve container references by name, service or id"`

### Task 3: Quoting, shell command and problem classification

**Files:** Modify `src/container.rs`

**Interfaces:**
- Produces:
  - `pub fn quote(word: &str) -> String`. Safe words (`[A-Za-z0-9_./:=@%+,-]+`) stay as they are. Anything else becomes `'…'`, with `'` written as `'\''`, and the empty word becomes `''`.
  - `pub fn remote_command<S: AsRef<str>>(words: &[S]) -> String`, the words quoted and joined by spaces.
  - `pub const SHELL_SCRIPT: &str = "command -v bash >/dev/null 2>&1 && exec bash || exec sh";`
  - `pub fn exec_words(id: &str, user: Option<&str>) -> Vec<String>`, which returns `["docker","exec","-it", ("-u", user)?, id, "sh", "-c", SHELL_SCRIPT]`
  - `pub enum DockerProblem { NotInstalled, DaemonDown, PermissionDenied, Other }`, deriving `Clone, Copy, Debug, Eq, PartialEq`
  - `pub fn classify(code: Option<i32>, stderr: &str) -> DockerProblem`

- [x] **Step 1: Write the failing tests:**
  - `quotes_only_when_needed`: `quote("web")=="web"`, `quote("a b")=="'a b'"`, `quote("o'brien")=="'o'\\''brien'"` and `quote("")=="''"`.
  - `remote_command_round_trips_through_sh`: for the words `["printf","%s|","a b","o'brien","","$HOME"]`, run `sh -c <remote_command>` and assert stdout is `a b|o'brien||$HOME|`.
  - `exec_words_with_and_without_user`
  - `classifies_docker_messages`:
    - code 127 with `bash: docker: command not found` gives NotInstalled
    - code 127 with `sh: 1: docker: not found` gives NotInstalled
    - `Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?` gives DaemonDown
    - `permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock: ...` gives PermissionDenied
    - `permission denied while trying to connect to the docker API at unix:///var/run/docker.sock` gives PermissionDenied
    - code 1 with `Error response from daemon: x` gives Other
- [x] **Step 2: Run the tests** and expect FAIL.
- [x] **Step 3: Implement.** Match case-insensitively on `cannot connect to the docker daemon`, `permission denied while trying to connect to the docker`, and, for code 127, `not found`.
- [x] **Step 4: Run the tests** and expect PASS.
- [x] **Step 5: Commit** with `git commit -m "feat: build container shell commands and classify docker failures"`

### Task 4: Group probe, decision and fix commands

**Files:** Modify `src/container.rs`

**Interfaces:**
- Produces:
  - `pub const PROBE_SCRIPT: &str`, a POSIX script printing `key=value` lines:
    - `user=$(id -un)`
    - `process=$(id -Gn)`
    - `database=$(id -Gn "$(id -un)")`
    - `group=yes|no` (from `getent group docker`)
    - `socket=<path>`: `${DOCKER_HOST:-unix:///var/run/docker.sock}` with `unix://` stripped, or empty for a non-unix host
    - `socket_group=$(stat -c %G <socket>)`
    - `sg=yes|no`, `newgrp=yes|no` and `systemctl=yes|no` (from `command -v`)
  - `pub struct Probe { pub user: String, pub process_groups: Vec<String>, pub database_groups: Vec<String>, pub group_exists: bool, pub socket: String, pub socket_group: Option<String>, pub has_sg: bool, pub has_newgrp: bool, pub has_systemctl: bool }`
  - `pub fn parse_probe(stdout: &str) -> Option<Probe>`, which returns `None` when `user` is missing.
  - `pub enum GroupTool { Sg, Newgrp }`, deriving `Clone, Copy, Debug, Eq, PartialEq, Hash`
  - `pub enum Access { RerunInGroup(GroupTool), NeedsLogin, OfferJoin, NoOffer }`
  - `pub fn decide(probe: &Probe) -> Access`:
    - `docker` in the database but not in the process gives Rerun (Sg preferred, then Newgrp), or NeedsLogin when neither tool exists.
    - Otherwise, not in the database and `socket_group` is `docker` or `root`, or the group is missing, gives OfferJoin.
    - Otherwise NoOffer.
  - `pub fn in_group(tool: GroupTool, words: &[String]) -> Vec<String>`, which returns `["sg","docker","-c",remote_command(words)]` or `["newgrp","docker","-c",remote_command(words)]`.
  - `pub const JOIN_SCRIPT: &str = "groupadd -f docker && usermod -aG docker \"$1\" && chgrp docker \"$2\"";`
  - `pub fn join_group_words(user: &str, socket: &str) -> Vec<String>`, which returns `["sudo","sh","-c",JOIN_SCRIPT,"sh",user,socket]`
  - `pub fn start_daemon_words() -> Vec<String>`, which returns `["sudo","systemctl","start","docker"]`

- [x] **Step 1: Write the failing tests:**
  - `parses_probe_output`
  - `decides_rerun_with_sg_then_newgrp_then_login`
  - `offers_join_when_socket_is_docker_or_root_or_group_missing`
  - `no_offer_for_other_socket_group_or_already_member`
  - `probe_script_runs_under_sh`: run `sh -c PROBE_SCRIPT` locally and assert `parse_probe` yields `Some` whose `user` equals the output of `id -un`.
  - `in_group_and_join_words`
- [x] **Step 2: Run the tests** and expect FAIL.
- [x] **Step 3: Implement** the items above.
- [x] **Step 4: Run the tests** and expect PASS.
- [x] **Step 5: Commit** with `git commit -m "feat: detect docker group access and build the fixes"`

### Task 5: OpenSSH variants for remote commands

**Files:** Modify `src/openssh.rs`

**Interfaces:**
- Produces:
  - `pub(crate) fn run_remote(&self, alias: &str, command: &str, batch: bool, cancelled: Option<&AtomicBool>) -> Result<Output, OpenSshError>`
    - Runs `ssh -o RemoteCommand=none [-o BatchMode=yes] -- <alias> <command>` and returns the `Output` whatever the exit status.
    - Batch: stdin null, run through the existing `cancellable_output` when `cancelled` is set.
    - Not batch: stdin inherited and `.output()` (no new process group, so ssh can prompt on the terminal).
  - `pub(crate) fn connect_mirrored_command(&self, alias: &str, command: Option<&str>, sink: Box<dyn Write + Send>, cancelled: Option<&AtomicBool>) -> Result<Mirrored, OpenSshError>`
    - With `Some(command)` it runs `ssh -t -o RemoteCommand=none -- <alias> <command>`. With `None` it runs exactly today's `ssh -- <alias>`.
    - The existing `connect_mirrored` becomes a one-line call with `None`.
  - `pub fn launch_new_terminal_with(&self, portico_args: &[OsString], fallback: &[OsString]) -> Result<ConnectionOutcome, OpenSshError>`
    - Runs `<terminal> <portico> <portico_args…>` when Portico's path is known, otherwise `<terminal> <fallback…>`.
    - `launch_in_new_terminal(alias)` calls it with `["connect","--hold","--",alias]` and `[ssh,"--",alias]`.
  - `pub fn ssh_program(&self) -> &Path`

- [x] **Step 1: Write the failing tests**, next to the existing `fake_ssh` tests:
  - `run_remote_passes_remote_command_none_and_batch`: assert the recorded arguments are `-o RemoteCommand=none -o BatchMode=yes -- dev docker ps`, and that a non-zero exit returns `Ok(output)` with its status.
  - `connect_mirrored_command_requests_a_tty`: assert the arguments are `-t -o RemoteCommand=none -- dev 'docker exec …'`.
  - `new_terminal_with_custom_portico_args`: assert the terminal receives `<portico> shell --hold x`, and the fallback arguments when Portico's path is unknown.
  - The existing connect and new-terminal tests stay green, unchanged.
- [x] **Step 2: Run** `cargo test --lib openssh::` and expect FAIL.
- [x] **Step 3: Implement** the items above.
- [x] **Step 4: Run** `cargo test --lib openssh::` and expect PASS.
- [x] **Step 5: Commit** with `git commit -m "feat: run remote commands through the ssh connect paths"`

### Task 6: App layer: targets, listing, shells and fixes

**Files:** Modify `src/app.rs` (new methods only, with types in a new section)

**Interfaces:**
- Consumes: Tasks 1 to 5.
- Produces, in `src/app.rs`:
  - `pub enum Target { Local, Host(String) }`, deriving `Clone, Debug, Eq, PartialEq, Hash`, with `pub fn describe(&self) -> String` returning `"this machine"` or the alias.
  - `pub struct ContainerList { pub target: Target, pub listing: Listing, pub via: Option<GroupTool>, pub note: Option<String> }`. When `via` is set, `note` reads ``Ran docker through `sg docker` (or `newgrp docker`): other shells need `newgrp docker` or a new login.``
  - `pub enum Offer { JoinGroup { user: String, socket: String }, StartDaemon }`, with `pub fn words(&self) -> Vec<String>` built from the Task 4 builders.
  - `pub struct DockerIssue { pub target: Target, pub problem: DockerProblem, pub headline: String, pub next: Vec<String>, pub offer: Option<Offer>, pub stderr: String }`
  - `pub enum ContainerError { App(AppError), Ssh { alias: String, explanation: Option<Explanation>, stderr: String }, Docker(DockerIssue), NoMatch { target: Target, reference: String }, Ambiguous { target: Target, reference: String, candidates: Vec<String> } }`, implementing `Display` with:
    - `no container matches {reference} on {target}`
    - `{reference} matches several containers on {target}: {a, b}`
    - for `Docker`, the headline
    - for `Ssh`, `ssh to {alias} failed`
  - `pub fn containers(&self, target: &Target, batch: bool, cancelled: Option<&AtomicBool>) -> Result<ContainerList, ContainerError>`
  - `pub fn container_shell(&self, target: &Target, id: &str, user: Option<&str>, via: Option<GroupTool>, new_terminal: bool, cancelled: Option<&AtomicBool>, in_tui: bool) -> Result<ConnectionOutcome, ContainerError>`
  - `pub fn docker_fix(&self, target: &Target, offer: &Offer) -> Result<ExitStatus, ContainerError>`
- Behaviour:
  - A host target is resolved through `connectable_alias`.
  - **Listing:** run `ps_words()`, either directly (`Command::new("docker")`, mapping `ErrorKind::NotFound` to `NotInstalled`) or through `run_remote(remote_command(..))`.
    - ssh status 255 with an empty stdout gives `Ssh`, with `explain_connection`.
    - Status 0 gives `parse_listing`.
    - Otherwise `classify`. On `PermissionDenied` or `DaemonDown`, run `PROBE_SCRIPT` on the same target and `decide`.
    - `RerunInGroup` reruns `in_group(tool, ps_words())` and returns `via`.
    - `OfferJoin` gives `Offer::JoinGroup`, and `DaemonDown` with `has_systemctl` gives `Offer::StartDaemon`.
  - **Issue copy, exact text:**

    | Case | Headline | Next lines |
    |---|---|---|
    | NotInstalled | `Docker is not installed on {target}, or it is not on the PATH of a non-interactive shell there.` | `Install Docker from the distribution's packages, then list again.` |
    | DaemonDown | `The Docker daemon is not running on {target}.` | With the offer: `Start it with: sudo systemctl start docker`. Without it: `Start the Docker daemon, then list again.` |
    | Join offer | `{user} cannot use Docker on {target}: permission denied on {socket}.` | `Add {user} to the docker group (root-equivalent access): {remote_command(join_group_words)}`, `Or use rootless Docker: https://docs.docker.com/engine/security/rootless/`, then `Or run sudo docker yourself in portico connect {alias}` (on a host) or `Or run sudo docker yourself` (locally) |
    | NeedsLogin | `{user} is in the docker group on {target}, but this session started before that and neither sg nor newgrp is available.` | `Log out and back in, then list again.` |
    | NoOffer | `Permission denied on {socket}, which belongs to group {group}.` | `Ask the administrator of {target} for access, or run sudo docker yourself.` |
  - **Shell:** words are `exec_words`, wrapped by `in_group` when `via` is set.
    - Local: `Command::new(words[0])` with the terminal inherited, returning `ConnectionOutcome::Exited { alias: "this machine", status, explanation: None }`.
    - Host: `connect_mirrored_command(alias, Some(remote_command(words)))`, with 255 explained exactly as `connect_here` does (`in_tui` chooses the wording).
    - New terminal: `launch_new_terminal_with`, with portico args `["shell","--hold",("--on",alias)?,("-u",user)?,("--via",sg|newgrp)?,"--",id]` and the ssh or docker words as the fallback.
  - **Fix:** runs `offer.words()` locally with the terminal inherited, or as `ssh -t` through `connect_mirrored_command`, and returns its status.

- [x] **Step 1: Write failing integration tests** in a new `tests/containers.rs`, using `TestEnv::install_program` fakes for `docker`, `ssh`, `sudo` and `sg` that record their arguments to files under `env.home()`. Drive them through the CLI commands from Tasks 7 and 8, so these tests are written there. This task's own unit test is `target_describe` in `app.rs`.
- [x] **Step 2: Implement** the items above. Most of the behaviour is proved by the tests in Tasks 7 and 8, which compile against these signatures.
- [x] **Step 3: Run** `cargo test --lib` and `cargo clippy --all-targets -- -D warnings`, and expect PASS.
- [x] **Step 4: Commit** with `git commit -m "feat: list containers and open shells on this machine or a host"`

### Task 7: `portico containers`

**Files:** Modify `src/cli.rs`. Create `tests/containers.rs`.

**Interfaces:**
- Consumes: `App::containers`, `group`, `display_name`, `ContainerError`, `Offer`.
- Produces:
  - `Command::Containers { #[arg(long = "on", value_name = "ALIAS")] on: Option<String> }`
  - `fn target(on: Option<String>) -> Target`
  - `fn print_listing(output, &ContainerList)`: a project header line, then rows indented two spaces with the columns `display  name  image  status` aligned per group. `Other` comes last. Then `note` and `skipped` (`Skipped {n} lines docker printed that Portico could not read.`) if present. With no containers: `No running containers on {target}.`
  - `fn list_with_offers(app, target, input, output, error, interactive) -> Result<ContainerList, String>`. On `Docker(issue)` it prints the headline and next lines to stderr. If there is an offer and the session is interactive, it asks `{Offer prompt} [y/N] ` with these prompts:
    - Join: `Add {user} to the docker group on {target}? Members of the docker group have root-equivalent access. This runs: {remote_command(words)}`
    - Start: `Start the Docker service on {target}? This runs: sudo systemctl start docker`

    On yes, it runs `docker_fix` and then lists once more. On `Ssh`, it prints like `print_connection_failure`.
- Exit codes: success 0, every error 1.

- [x] **Step 1: Write the failing tests** in `tests/containers.rs`:
  - `lists_local_containers_grouped`: a fake `docker` prints two `shop` lines and one plain line. Assert stdout contains `shop`, `  db`, `  web` and `Other`, with the rows in that order.
  - `lists_remote_containers_over_ssh`: init plus `add dev -H dev.example`, and a fake `ssh` that records its arguments and prints the lines. Assert the arguments contain `-o RemoteCommand=none -- dev docker ps --no-trunc --format '{"id":…'` (quoted), and that `BatchMode` is absent.
  - `alias_case_uses_inventory_spelling`: `--on DEV` records `-- dev`.
  - `no_containers_message`
  - `docker_missing_is_explained`: `PATH` without `docker` prints the NotInstalled headline and exits 1.
  - `daemon_down_offers_start_and_declines_non_interactively`
  - `permission_denied_rerun_through_sg`: the fake `docker` fails with permission denied unless run through `sg`. Locally the probe runs `sh -c PROBE_SCRIPT`, so install fake `id`, `getent`, `stat` and `sg` programs that report docker in the database but not in the process. Assert the listing prints and the note mentions `sg docker`.
  - `join_offer_accepted_runs_sudo_then_lists`: an interactive run (`run_with_stdin`, with interactivity forced the same way the existing remove-confirmation tests do), answering `y`. The fake `sudo` records its arguments, which must equal the `join_group_words`.
  - `join_offer_declined_lists_options`
- [x] **Step 2: Run** `cargo test --test containers` and expect FAIL.
- [x] **Step 3: Implement** the items above.
- [x] **Step 4: Run** `cargo test --test containers` and expect PASS.
- [x] **Step 5: Commit** with `git commit -m "feat: add portico containers"`

### Task 8: `portico shell` with picker

**Files:** Modify `src/cli.rs` and `tests/containers.rs`

**Interfaces:**
- Produces:
  - `Command::Shell { container: Option<String>, #[arg(long = "on")] on: Option<String>, #[arg(short = 'u', long)] user: Option<String>, #[arg(short = 'n', long = "new-terminal")] new_terminal: bool, #[arg(long, hide = true)] hold: bool, #[arg(long, hide = true, value_enum)] via: Option<Via> }`, where `Via` is `{Sg, Newgrp}` and maps to `GroupTool`.
  - `fn pick(list: &ContainerList, input, output) -> Result<Option<String>, String>`, which returns the chosen ID, or `None` on cancel.
    - When `fzf` is on PATH, spawn it with `--delimiter '\t' --with-nth 2.. --prompt 'container> '` and stdin lines `id\tproject-or-Other\tdisplay\tname\timage\tstatus`. Exit 130 or 1 means `None`.
    - Otherwise print the numbered rows `{n}) {project}  {display}  {name}  {image}  {status}` and the prompt `Container number: `. Empty input or EOF means `None`, and a bad number is the error `no container numbered {x}`.
- Flow:
  - With `--hold` and an ID, the shell runs directly, without listing.
  - Otherwise list with offers. An empty listing gives `No running containers on {target}.` and exit 1.
  - With a reference, `resolve`, mapping its errors to `NoMatch` and `Ambiguous`. Without one, `pick` when interactive. Otherwise the error is `choose a container: portico shell <container> (no terminal for the picker)`, and a cancelled pick exits 130.
  - Then `container_shell`. The outcome is handled exactly like `Command::Connect`: explanation, `--hold` wait, and `exit_code`.

- [x] **Step 1: Write the failing tests:**
  - `shell_local_by_service`: the fake `docker` records the `exec` arguments: `exec -it <full id> sh -c 'command -v bash…'`.
  - `shell_with_user`: `-u o'brien` on a host, and the ssh arguments contain `-t -o RemoteCommand=none -- dev` followed by the quoted remote command containing `-u 'o'\''brien'`.
  - `shell_ambiguous_lists_candidates` and `shell_unknown_reference`
  - `shell_exit_code_passes_through`: the fake `docker exec` exits 1 and then 130, Portico exits with the same code, and stderr carries no Docker headline.
  - `shell_ssh_255_is_explained`: reuse the connect-failure fixture text (`Permission denied (publickey)`) and assert the explanation headline from `connect`.
  - `shell_new_terminal_passes_resolved_id`: the fake `xdg-terminal-exec` records `<portico> shell --hold --on dev -- <id>`.
  - `numbered_picker_selects_second`: no `fzf` on PATH and stdin `2\n` gives an exec of the second row's ID. Empty stdin gives exit 130 and no exec.
  - `fzf_picker_used_when_present`: a fake `fzf` echoes its second stdin line, and the exec uses that ID.
  - `no_containers_shell_exits_one`
- [x] **Step 2: Run** `cargo test --test containers` and expect FAIL.
- [x] **Step 3: Implement** the items above.
- [x] **Step 4: Run** `cargo test --test containers` and expect PASS.
- [x] **Step 5: Commit** with `git commit -m "feat: open a container shell from the CLI with a picker"`

### Task 9: TUI containers panel (state, keys, rendering)

**Files:** Create `src/tui/containers.rs`. Modify `src/tui.rs` (`mod containers;` only, in this task).

**Interfaces:**
- Consumes: `Target`, `ContainerList`, `ContainerError`, `group`, `display_name`, and `TextInput` from the parent module.
- Produces:
  - `pub(super) enum PanelState { Unlisted, Listing, Listed(ContainerList), Failed(PanelFailure) }`, where `PanelFailure { lines: Vec<String>, offer: Option<Offer> }`.
  - `pub(super) struct Panel { pinned_local: bool, host: Option<String>, lists: HashMap<Target, PanelState>, selected: usize, user: TextInput, editing_user: bool }`
  - `pub(super) enum PanelAction { None, Leave, Quit, List(Target), CancelList, Shell { target: Target, id: String, user: Option<String>, via: Option<GroupTool>, new_terminal: bool }, Fix { target: Target, offer: Offer } }`
  - Methods:
    - `fn target(&self) -> Option<Target>`: `Local` when pinned, otherwise the followed host.
    - `fn follow(&mut self, alias: Option<&str>)`: sets the host and unpins.
    - `fn on_focus(&mut self) -> PanelAction`: `List(target)` when the target is `Unlisted`.
    - `fn handle_key(&mut self, key: KeyEvent) -> PanelAction`
    - `fn set_listing(&mut self, target: Target)`
    - `fn finish(&mut self, target: Target, result: Result<ContainerList, ContainerError>)`
    - `fn title(&self) -> String`: `Containers: {target}`, or `Containers` without a target.
    - `fn lines(&self, focused: bool) -> Vec<Line<'static>>`: the project header rows, the rows marked `>` when selected, `Tab to list containers`, `Listing…`, or the failure lines followed by `g add to docker group` or `s start docker`.
    - `fn details(&self) -> Option<String>`: the selected container's name, id, image, status, running for, ports, project, service, working dir, networks, mounts and command, one `label: value` line each, skipping empty values. Without `-u`, user reads `image default`.
    - `fn footer(&self) -> &'static str`: `↑↓ choose  Enter shell  n new terminal  u user  r refresh  L this machine  Tab hosts`. While editing the user: `Enter apply  Esc cancel`.
  - Keys follow the spec table:
    - Up, Down, j and k skip headers.
    - Enter and `n` give `Shell`, with `user` empty mapped to `None`.
    - `u` starts editing. Enter applies the user, and Esc restores the previous one.
    - `r` gives `List`. `L` pins local, then gives `on_focus`.
    - `g` and `s` work only when the failure's offer matches.
    - Tab, Shift+Tab (`BackTab`) and Esc give `Leave`. While listing, Esc gives `CancelList` and also leaves.
    - `q` gives `Quit`.

- [x] **Step 1: Write the failing tests** in `src/tui/containers.rs` `mod tests`:
  - `focus_lists_once_per_target`: `on_focus` gives `List`, then after `finish`, `on_focus` gives `None`.
  - `follow_unpins_local`
  - `selection_skips_headers`
  - `enter_and_n_produce_shell_with_user`: the user is set via `u`, typing `root` and Enter.
  - `esc_while_editing_user_restores`
  - `offer_keys_only_for_matching_offer`
  - `renders_groups_and_hint`
  - `details_for_selected_container`
  - `failure_lines_for_ssh_and_docker`
- [x] **Step 2: Run** `cargo test --lib tui::containers` and expect FAIL.
- [x] **Step 3: Implement** the items above.
- [x] **Step 4: Run the tests** and expect PASS.
- [x] **Step 5: Commit** with `git commit -m "feat: add the TUI containers panel"`

### Task 10: TUI wiring: layout, focus, worker, shells and fixes

**Files:** Modify `src/tui.rs`. Create `src/tui/lister.rs` (the worker).

**Interfaces:**
- Consumes: the `Panel` from Task 9, plus `App::containers` (batch), `container_shell` and `docker_fix`.
- Produces:
  - `src/tui/lister.rs`: `pub(super) struct Lister`, with `start(&mut self, target: Target)`, `cancel(&mut self)` and `try_finished(&mut self) -> Option<(Target, Result<ContainerList, ContainerError>)>`. It is a single worker thread with generation counting and a cancel flag, as in `tester.rs`, calling `app.containers(&target, true, Some(&cancelled))`.
  - In `UiState`: `containers: Panel` and `focus: Focus { Hosts, Containers }`.
    - In `Mode::Browse`, `Tab` sets the focus to Containers and returns `UiAction::Containers(panel.on_focus())`. While the panel is focused, keys go to `panel.handle_key`, and the result is wrapped in `UiAction::Containers`.
    - Host selection changes call `panel.follow(selected alias)`.
  - `UiAction::Containers(PanelAction)`, handled in `browse_hosts`:
    - `List`: `lister.start` and `panel.set_listing`.
    - `CancelList`: `lister.cancel`.
    - `Leave`: focus Hosts.
    - `Shell`: either `suspend_for_child` with `container_shell(.., in_tui = true)` and `redraw_from_scratch`, as for Connect, or the new terminal directly. The result is shown in the status line: `Shell on {target} exited with {status}` or the explanation's headline.
    - `Fix`: `suspend_for_child` around `docker_fix`, then `List` again.
  - **Render:** the left column splits vertically into hosts (`Min(3)`) and containers (`Length(max(6, 40% of height))`). The details pane shows `panel.details()` while the panel is focused, and the footer shows `panel.footer()` while it is focused. The browse footer gains `Tab containers`.
  - The worker's results are polled in the same loop iteration that polls `tester`.

- [x] **Step 1: Write the failing tests** in `src/tui.rs` tests:
  - `tab_focuses_containers_and_lists`: `press(Tab)` gives `UiAction::Containers(PanelAction::List(Target::Host("dev")))`.
  - `panel_keys_route_when_focused`: `j` moves the panel's selection and leaves the host selection unchanged.
  - `moving_host_selection_follows`
  - `details_pane_shows_container_when_focused`: rendered with `TestBackend`, as the existing render tests are.
  - `browse_footer_mentions_tab_containers`
  - Every existing TUI test stays green.
- [x] **Step 2: Run** `cargo test --lib tui::` and expect FAIL.
- [x] **Step 3: Implement** the items above.
- [x] **Step 4: Run** `cargo test --all-targets` and expect PASS.
- [x] **Step 5: Manual check** of `cargo run` with the local Docker (`pgadmin` is running): Tab, `L`, the details, Enter opening a shell, and `exit` returning to the TUI.
- [x] **Step 6: Commit** with `git commit -m "feat: list containers and open shells from the TUI"`

### Task 11: Documentation and final checks

**Files:** Modify `README.md`

- [x] **Step 1:** Edit the README:
  - Add `portico containers [--on <alias>]` and `portico shell [<container>] [--on <alias>] [-u <user>] [-n]` to the CLI table, with examples.
  - Add a "Container shells" paragraph: Docker only, nothing stored, the reference forms, the fzf picker, and the offers (group join, daemon start, `sg`/`newgrp`).
  - Add the containers panel's keys to "TUI keys".
  - Add the optional `docker` and `fzf` to Requirements.
- [x] **Step 2:** Run `scripts/ci-checks.sh fmt`, `scripts/ci-checks.sh clippy`, `scripts/ci-checks.sh test` and `scripts/ci-checks.sh docs` as the regular user, and expect all to pass.
- [x] **Step 3: Commit** with `git commit -m "docs: document container shells"`
