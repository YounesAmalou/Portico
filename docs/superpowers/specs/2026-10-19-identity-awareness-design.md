# Identity Awareness Design

**Status:** Approved

## Purpose

Portico shows a host's IdentityFile as a bare path today. It cannot say whether that file exists, whether the key is already held by an agent (1Password's SSH agent, `ssh-agent`, gpg-agent), or which agent the host would use at all. A user who keeps keys in 1Password, with only public keys on disk, sees paths that look broken but work, and a user with a typo in a path sees one that looks fine but fails.

This slice makes Portico **aware** of identities: for every host it shows which identities OpenSSH would offer, where each one lives, and whether it is available right now. The CLI gains an `identities` command that lists every identity Portico knows of, and the TUI host form suggests those identities while the IdentityFile field is typed.

It implements v3 item 4 of `ROADMAP.md`, "Identity awareness", and the first follow-up slice of the v1 design. **Knowledge stays separate from possession:** Portico learns paths, fingerprints and comments, never key material.

## Settled questions

- **Discovery and display only.** Portico never reads a private key file, never generates a key, never loads a key into an agent, never removes one, and never asks an agent to sign. It runs `ssh-add -l` (which asks an agent for its public key list) and `ssh-keygen -l` on public key files only.
- **No new state.** Nothing is written to disk. The agent listing is kept in memory for one TUI session (until refresh), and recomputed by every CLI command.
- **OpenSSH and its tools do the work.** The effective identity configuration comes from `ssh -G`, agent keys from `ssh-add`, fingerprints from `ssh-keygen`. Portico adds no cryptography or agent-protocol code and no new dependency.
- **Availability is not a promise.** Portico reports what is present, not whether the server will accept it. `portico test` remains the way to check authentication.
- **The managed host model does not change.** A managed host still declares at most one IdentityFile as a path reference.

## Decisions

### Sources

| Source | How it is read | What Portico learns |
|---|---|---|
| A host's effective configuration | `ssh -G <alias>`, already run for host details; the parser also keeps `identityagent`, `identitiesonly` and `pkcs11provider` | The IdentityFile list in order, the agent socket, whether only listed identities are offered, a PKCS#11 provider path |
| OpenSSH's built-in defaults | `ssh -G -F none portico-defaults`, once per process | The identity files OpenSSH tries when none is configured, for this OpenSSH version |
| The agent | `ssh-add -l -E sha256` with `SSH_AUTH_SOCK` set to the socket, stdin null, a 3 s deadline | Each key's type, size, SHA-256 fingerprint and comment |
| Public key files | `ssh-keygen -l -E sha256 -f <file.pub>`, same deadline | The fingerprint, type and comment of the key the file pairs with |
| Private key files | `stat` only (`std::fs::metadata`): never opened | Whether the file exists as a regular file |
| `~/.ssh/*.pub` | A non-recursive directory listing of `~/.ssh` (not `~/.ssh/portico`), names ending in `.pub` but not `-cert.pub` | Key pairs and agent-backed public keys that no host references yet, for `portico identities` and the form suggestions |

**Why `ssh-add` and not the agent protocol.** Speaking the protocol needs a Unix socket client, a parser for the binary key blobs and a SHA-256 implementation; `ssh-add -l` already prints exactly the fingerprint and comment, uses OpenSSH's own understanding of every key type (including FIDO `sk-` keys and post-quantum hybrids such as `ssh-mldsa44-ed25519`), and is installed wherever `ssh` is. Fingerprints are compared as `ssh-add` and `ssh-keygen` print them, so the two sides always agree.

**Why `-F none` for defaults.** `ssh -G` prints OpenSSH's default identity list for a host with no IdentityFile, indistinguishable from a configured list. `ssh -F none` reads no configuration file, so its list is exactly the build's defaults. A host whose list equals it is shown as "OpenSSH defaults" (a configured list identical to the defaults behaves identically, so the label is never wrong in effect).

### Resolving the agent

`ssh -G` prints `identityagent` already expanded (`~` and environment variables). Portico maps it to a socket:

| `identityagent` | Socket |
|---|---|
| absent, or `SSH_AUTH_SOCK` | Portico's own `SSH_AUTH_SOCK`; none when unset or empty |
| `none` | No agent: shown as "disabled by IdentityAgent none" |
| any other value | That path |

A socket path that does not exist, or is not a socket, is reported without running `ssh-add`. A listing that times out is reported as "did not answer" with the hint that the agent may be waiting to be unlocked; Portico never retries on its own.

**Locked and confirm-on-use agents.** A locked 1Password, or an agent that confirms each use, may show an unlock prompt for a list request or never answer. Portico therefore:

- runs `ssh-add` with `SSH_AUTH_SOCK` set to the resolved socket, `SSH_ASKPASS` removed and `SSH_ASKPASS_REQUIRE=never`, and stdin null, so the listing itself can never start a prompt from Portico;
- kills the `ssh-add` child when the 3 s deadline passes, and reaps it, so no listing outlives its request;
- in the TUI, asks each socket at most once per session (and once more after each refresh), in the background, and never from the host list: moving the cursor reuses the listing once a socket has been asked. A socket whose listing failed or timed out is not asked again until refresh.

**Provider label.** The socket path gives the agent a friendly name, for display only:

- `…/.1password/agent.sock`, or a path containing `com.1password` (macOS group container): **1Password**
- a file named `S.gpg-agent.ssh`: **gpg-agent**
- a path containing `/keyring/` (GNOME Keyring): **GNOME Keyring**
- anything else: **SSH agent**

The label never changes behaviour; an unknown agent works exactly like a known one.

**Environment caveat.** The agent named by `SSH_AUTH_SOCK` is the one in Portico's environment. A TUI started from a desktop launcher may see a different agent than a terminal; the details show the socket path, so the difference is visible.

### Expanding identity paths

`ssh -G` prints IdentityFile values **without** expanding `~`, `%` tokens or `${VAR}` (checked with OpenSSH 10.x). To check a file Portico expands, in the same way ssh does for IdentityFile:

- a leading `~/` (and `~` alone) to `$HOME`
- `%d` (home), `%u` (local user), `%h` (the effective HostName), `%r` (the effective User), `%p` (the effective port), `%n` (the alias as given), `%i` (local uid), `%%`
- `${VAR}` from Portico's environment

Any other token (`%C`, `%k`, `%l`, `%L`, `%j`, `%T`…) or an undefined `${VAR}` leaves the identity **unchecked**, shown as such with the token named. Portico never guesses. Paths are always displayed as configured, with `$HOME` shown as `~`.

`IdentityFile none` produces no identities.

### What each identity's availability means

Each identity in a host's list is classified once, in this order:

1. **Unchecked:** the path cannot be expanded (above).
2. Find the public key: the path itself when it ends in `.pub` (OpenSSH accepts a public key as IdentityFile to select an agent key, the pattern 1Password documents), otherwise `<path>.pub`. When it exists as a regular file, `ssh-keygen -l` gives its fingerprint.
3. **In agent:** the host's agent was listed and holds a key with that fingerprint. This holds whether or not the private file exists.
4. **Key file:** the private file exists (step 2's `.pub` is not required). ssh can load it, possibly asking for a passphrase. When no `.pub` exists, the line adds "agent not checked: no public key file".
5. **Missing:** neither of the above. The line says what was found: "public key only, and the agent does not hold it", "public key only; agent unavailable", or "no such file".

For an identity that is a `.pub` path, step 4 never applies: a public key alone cannot sign.

**OpenSSH defaults** are shown collapsed: only the default files that exist are listed (classified as above), followed by "other OpenSSH defaults: none present". A missing default is not an error; ssh skips it silently, and listing six of them would bury the real information.

**Keys the agent offers in addition.** With `IdentitiesOnly no` (the default), ssh also offers every agent key not already listed. Their count and comments are shown as "also offered by the agent". With `IdentitiesOnly yes` the line reads "IdentitiesOnly yes: the agent offers only the identities above".

**PKCS#11.** A `PKCS11Provider` other than `none` is shown as "PKCS#11 provider: <path>". It is not probed (that would load the library).

**Certificates** (`CertificateFile`) are out of scope; see "Out of scope".

### Data model

A new module `src/identity.rs` holds plain data with no key material and no I/O in its types, so the later `--json` slice can serialize it as it is:

```rust
pub struct PublicKey {          // one line of ssh-add -l or ssh-keygen -l
    pub key_type: String,       // "ED25519", "RSA", "ED25519-SK", ...
    pub bits: u32,
    pub fingerprint: String,    // "SHA256:..."
    pub comment: String,
}

pub enum Provider { OnePassword, GpgAgent, GnomeKeyring, Generic }

pub enum Agent {
    Disabled,                                  // IdentityAgent none
    NotConfigured,                             // no SSH_AUTH_SOCK
    Unavailable { socket: PathBuf, reason: AgentProblem },  // missing, not a socket, refused, no answer
    Listed { socket: PathBuf, provider: Provider, keys: Vec<PublicKey> },
}

pub enum Availability {
    InAgent,
    KeyFile { public_key_found: bool },
    Missing(MissingReason),
    Unchecked { token: String },
}

pub struct Identity {
    pub configured: String,          // the IdentityFile value as ssh -G printed it
    pub path: Option<PathBuf>,       // expanded, when it could be
    pub public_key: Option<PublicKey>,
    pub availability: Availability,
}

pub struct HostIdentities {
    pub agent: Agent,
    pub defaults: bool,              // the list is OpenSSH's built-in default list
    pub identities: Vec<Identity>,   // with defaults, only the files that exist
    pub identities_only: bool,
    pub also_offered: Vec<PublicKey>,  // agent keys not matched above; empty when identities_only
    pub pkcs11_provider: Option<String>,
}
```

The fingerprint and comment of a public key are what `ssh-add -l` prints to anyone with the socket; they identify a key and grant nothing. The base64 public key blob is not kept. The `--json` slice serializes `HostIdentities` and the inventory as they are, so the field and variant names above are chosen to be stable: they are part of that future contract, and renaming one later is a breaking change.

`HostDetails` gains `identities: Option<HostIdentities>` (absent when resolution failed). The pure classification lives in `identity.rs` and is unit-tested against fake listings; the process calls live in `openssh.rs` beside `resolve`.

### The identity inventory

`portico identities` and the form suggestions draw from one inventory, built by `app::identity_inventory`:

- every IdentityFile referenced by any host (managed or external) through `ssh -G`, excluding OpenSSH defaults that do not exist, with the aliases that use it;
- every `~/.ssh/*.pub` and its private counterpart's presence;
- every distinct agent socket used by any host, plus `SSH_AUTH_SOCK`, and the keys it lists.

Entries are keyed by expanded path, so `~/.ssh/a` and `/home/me/.ssh/a` are one entry; each shows the first configured spelling.

Resolving every host costs one `ssh -G` per host (a few milliseconds each). It runs only for `portico identities` and, in the TUI, in the background when a form opens; the host list never pays for it.

### CLI

**`portico show <alias>`** gains an identities block after the resolved fields, in the existing `Label: value` style:

```
Agent: 1Password, ~/.1password/agent.sock, 3 keys
Identity: ~/.ssh/github.pub: in agent (ED25519 SHA256:3/9rWBEc… "GitHub")
Identity: ~/.ssh/id_work: missing (no such file)
IdentitiesOnly: yes
```

and, for a host on OpenSSH defaults with an ordinary agent:

```
Agent: SSH agent, /run/user/1000/ssh-agent.socket, 1 key
Identity: ~/.ssh/id_ed25519 (OpenSSH default): key file, in agent (ED25519 SHA256:… "me@laptop")
Identity: other OpenSSH defaults: none present
Also offered by the agent: 0
```

Every line `show` prints today stays byte-for-byte unchanged (`Resolved IdentityFile` included), so scripts reading `show` keep working; the identities block is only appended after them, before the warnings. Fingerprints are printed in full; the TUI shortens them.

**`portico identities`** (new) prints the inventory, agents first:

```
Agents
  1Password  ~/.1password/agent.sock  3 keys  (SSH_AUTH_SOCK)
    ED25519  SHA256:NQlIU/kf…  VPS
    ED25519  SHA256:3/9rWBEc…  GitHub          ~/.ssh/github.pub
    ED25519  SHA256:9K4JN0kK…  codex-work

Identity files
  ~/.ssh/github.pub    in agent   used by: github.com, gist
  ~/.ssh/id_work       missing    used by: work
  ~/.ssh/old_ed25519   key file   used by: -
```

- Agent keys show the public key file that pairs with them, when one is known.
- An agent that cannot be listed is shown with its reason instead of keys.
- The command always exits 0 unless the inventory itself cannot be read (as `portico hosts`). Missing identities are information, not failures.
- `--json` is not added here; it belongs to the machine-readable slice.

### TUI

**Host details.** Under the resolved fields, the details pane shows an **Identities** section in the same wording as `show`, condensed:

```
Agent        1Password · 3 keys
Identity     ~/.ssh/github.pub    ✓ in agent  GitHub
             ~/.ssh/id_work       ✗ missing
Only listed  yes
```

- `✓` (green) for in agent or key file, `✗` (red) for missing, `?` (yellow) for unchecked; the words are always present, so colour is never the only signal.
- Fingerprints are shortened to `SHA256:` plus 8 characters with `…`; the comment is shown when it fits.
- The IdentityFile row of the resolved fields is replaced by this section (it showed the same paths without status). The "Declared by Portico but overridden" note keeps comparing paths as today.
- The identities are computed by the existing details worker, right after `ssh -G`, so the UI never waits for an agent: the details pane reads `Resolving…` as it does today until both are done. Because each socket is asked once per session, only the first host shown with a given agent can wait for it (at most the 3 s deadline); every later host reuses the listing.
- An agent listing is not interrupted when the selection moves on (the host's `ssh -G` still is): it runs to its result or its deadline and fills the cache, so moving the cursor can never cause the same agent to be asked twice.
- **Agent cache.** The first listing of a socket is kept for the session and shared by every host using it. `r` / `F5` (refresh) discards it with the other cached details, so loading a key into an agent and pressing `r` shows the change.

**Host form suggestions.** The IdentityFile field of the add and edit forms gets the dropdown the ProxyJump field already has, with the same open, close and key rules (typing opens it when something matches, Ctrl+Space opens it on demand, Enter or Tab picks, Esc closes):

- **Candidates:** the identity inventory's files: referenced identity files, `~/.ssh` key pairs, and `~/.ssh/*.pub` files whose key an agent holds. A `.pub` file whose private file exists is offered as the private path (what ssh expects); one without a private file is offered as the `.pub` path, the form that selects an agent key.
- **Query:** the whole field value (IdentityFile holds one path); matching is the same case- and accent-insensitive contains, prefix first, as for jump hosts, applied to the `~`-form path.
- **Each row** shows the path and a short origin: `in agent · 1Password`, `key file`, `used by 3 hosts`, or `missing`. Missing files are still offered (another host may use the same path), last.
- A pick inserts the path in `~/` form when it is under `$HOME`.
- **Loading:** the `~/.ssh` scan is immediate when the form opens; referenced files and agent keys arrive from a background worker (one `ssh -G` per host). Until it finishes, the dropdown offers what it has; it is recomputed when the worker returns, as it is when the inventory is replaced.
- The footer advertises `Ctrl+Space suggest` when IdentityFile is focused, as it does for ProxyJump.
- Free text stays valid: a typed path that matches nothing is saved and validated by OpenSSH as today.

## Code layout

- **`src/identity.rs` (new, pure):** the types above; `expand(path, context) -> Result<PathBuf, Token>`; `provider(socket)`; `parse_listing(text) -> Vec<PublicKey>` for both `ssh-add -l` and `ssh-keygen -l` lines (bits, fingerprint, comment with spaces, `(TYPE)`); `classify(...)` building `HostIdentities` from the resolved config, the defaults, file presence and listings; the inventory merge; candidate building for the form.
- **`src/openssh.rs`:** `ResolvedConfig` gains `identity_agent`, `identities_only`, `pkcs11_provider`; `OpenSsh` gains an `ssh-add` program path (`with_ssh_add`, like `with_keygen`) and `default_identity_files()`. Nothing here opens a file.
- **`src/app/identities.rs` (new):** the process side: `list_agent(socket)` and `fingerprint(pub_path)` with the deadline and the `ssh-add` environment above, the `~/.ssh` scan, `App::identity_inventory()`, and the agent cache (socket → listing) that `App` owns, so a CLI command asks once per process and the TUI once per session; `App::forget_identities()` empties it on refresh. `src/app.rs` only fills `HostDetails::identities` in `show`.
- **`src/cli.rs`:** the identities block in `show`, and the `identities` command. The new command is one enum variant and one function, to keep the merge with the tunnels slice's new subcommands trivial.
- **`src/tui/identities.rs` (new):** the details section's lines and the form's inventory worker. `src/tui.rs` gains only the hooks: the section in `detail_text`, the IdentityFile field in the existing suggestion code (which becomes field-aware), and the cache reset on refresh.
- **`src/suggest.rs`:** `Origin` gains `Note(String)`, a label shown as it is, so `rank` serves identity paths unchanged.
- **`README.md`:** `portico identities`, the identities lines of `show`, the details section, the IdentityFile suggestions, and a short paragraph on what Portico does and does not touch (1Password included).

## Out of scope

- Generating, importing, loading (`ssh-add`), removing or exporting keys; writing `.pub` files.
- Assigning identities to hosts beyond the existing IdentityFile field; IdentitiesOnly and IdentityAgent stay unmanaged directives (the "More OpenSSH directives" item decides that).
- `CertificateFile` and certificate validity.
- Probing PKCS#11 providers or security keys.
- 1Password's `agent.toml` (which vaults and keys the agent exposes): Portico sees what the agent lists, which is the effective result.
- Remote agents (ForwardAgent) and per-host identity indicators in the host list.
- `--json` output (the machine-readable slice consumes these types).

## Testing

- **Unit (`identity.rs`):** expansion of `~`, each supported token, `${VAR}` set and unset, unsupported tokens; provider labels for 1Password (Linux and macOS paths), gpg-agent, GNOME Keyring, others; listing parser with spaces in comments, empty comments, `sk` and hybrid types, `The agent has no identities.`; classification for every row of the availability order, including a `.pub` IdentityFile, a private file without `.pub`, defaults collapsed, `IdentitiesOnly` on and off, `IdentityFile none` and `IdentityAgent none`; inventory merge by expanded path with "used by".
- **Unit (`openssh.rs`):** the extended `ssh -G` parser, against captured output; fake `ssh-add`/`ssh-keygen` scripts for exit statuses 0, 1 and 2 and a deadline overrun: a fake `ssh-add` that sleeps past the deadline is killed, and the test asserts its process is gone; another fake records its environment to prove `SSH_AUTH_SOCK`, `SSH_ASKPASS_REQUIRE=never` and the absence of `SSH_ASKPASS`. `show` keeps every existing line byte-for-byte, asserted against the output before this slice.
- **TUI (unit):** the section's lines and symbols, the cache reset on refresh, the IdentityFile dropdown (opens on typing, Ctrl+Space, pick inserts `~/` form, `.pub` vs private offering, recomputed when the worker returns), ProxyJump suggestions unchanged.
- **End to end (`tests/e2e.rs`):** a real `ssh-agent -a <dir>/agent.sock` started for each test and killed afterwards (in a short temporary directory: a Unix socket path is limited to 108 bytes), keys created by the test with `ssh-keygen -N ''` (test set-up, not Portico) and loaded with `ssh-add`. With hosts whose IdentityAgent points at that socket, the tests prove: `show` reports in agent, key file and missing correctly; a `.pub`-only identity whose key is in the agent is "in agent" (the 1Password pattern); `IdentityAgent none` reports disabled; an agent socket that was removed reports unavailable; `portico identities` lists the agent keys and the files with "used by". A guard test proves Portico never opens a private key file: the private key is made unreadable (`chmod 000`) and every result stays the same.

## Decided product questions

Settled during review, under the maintainer's delegation:

1. **Saving an agent key as a `.pub` file** (to pin a host to one 1Password key): not in this slice. It is recorded in `ROADMAP.md` as an unscheduled item, because it would be the first file Portico writes outside its own directory.
2. **An identity indicator in the host list:** not in this slice and not planned. The details pane and `portico identities` cover it on demand, and the list never runs `ssh -G` for every host.
