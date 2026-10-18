# Connection Test Edge Cases Design

**Status:** Approved; implemented

## Purpose

The first version of `portico test` (`2026-10-17-connection-test-design.md`) mishandles three rare configurations. This slice fixes them and changes nothing else in that design. It implements item 1 of the v2 section of `ROADMAP.md`.

1. **Multi-hop jump with a verbose hop.** With `ProxyJump a,b`, hop `b` is itself reached through a proxy. When `b`'s configuration sets `LogLevel VERBOSE`, its own login line takes the `(via proxy)` form and comes before the target's, so the test takes it for the target's success.
2. **`ProxyUseFdpass yes`.** The `ProxyCommand` hands ssh a connected socket, so OpenSSH logs the direct form of the login line (`([<address>]:<port>)`). The test accepts only the `(via proxy)` form for a proxied host and reports a timeout.
3. **A late signal in `portico test`.** One cancel flag serves both the first test and the retest after trusting a key. A signal that arrives after the first test stops checking the flag, but before Portico restores the signals' default action, leaves the flag set. The retest then appends the key and immediately reports "cancelled".

## What OpenSSH logs

Recorded with OpenSSH 10.6p1 against a throwaway `sshd` listening on `127.0.0.1`, `127.0.0.2` and `127.0.0.3`:

- `ProxyJump hopa,hopb` with both hops at `VERBOSE` (`tests/fixtures/ssh/authenticated-via-two-jumps.txt`):
  ```
  Authenticated to 127.0.0.1 ([127.0.0.1]:22999) using "publickey".
  Authenticated to 127.0.0.2 (via proxy) using "publickey".
  Authenticated to 127.0.0.3 (via proxy) using "publickey".
  ```
- `ProxyCommand` with `ProxyUseFdpass yes` (`tests/fixtures/ssh/authenticated-via-fdpass.txt`):
  ```
  Authenticated to 127.0.0.3 ([127.0.0.3]:22999) using "publickey".
  ```

The host in each line is that hop's resolved `HostName`, which OpenSSH lowercases. `ssh -G` reports the target's `hostname` the same way. It also reports `proxyusefdpass`, which it forces to `no` whenever `ProxyJump` is set, so file-descriptor passing only ever applies to a `ProxyCommand`.

## Decisions

### The target's login line

- A success line counts as the target's only when its host equals the target's resolved `hostname`, ignoring ASCII case. This applies to proxied routes. A direct connection has no other logging process, so its rule is unchanged.
- `Route::Proxied` gains `fd_passing: bool`, read from `proxyusefdpass` in `ssh -G` into `ResolvedConfig`. A proxied route without fd passing accepts only the `(via proxy)` form, as today. One with fd passing accepts only the direct form. The address and port it reports then come from the line, as for a direct login, and `via` stays `proxy`.
- A success line from another hop is ordinary output: the test keeps reading, and the line stays in the collected stderr.
- **Known limit:** a later hop whose `HostName` equals the target's (two servers reached on one address through different ports, say) and that logs at `VERBOSE` can still be taken for the target. Its line carries no port, and nothing in ssh's output tells the two apart. The limit is recorded in a comment on `authenticated`.

### Cancellation in `portico test`

- `Interrupts::during` clears the cancel flag before it runs a step, so a flag left from an earlier step never cancels a later one.
- When the flag is set after a step that was not itself cancelled, a signal arrived late. Portico acts on it at the next step boundary:
  - After the first test found an unknown host key, Portico prints the host key and fingerprint as usual, then `<alias>: cancelled` and exits 130. It neither prompts nor writes anything.
  - After any other first-test result, and after the retest, that step's result is printed with its usual exit status. No further step was due, and after a retest the key is already trusted.
- The TUI already gives every test its own flag and is unchanged.

## Code layout

- `src/probe.rs`: `Route::Proxied { fd_passing }`. `authenticated` checks the host and picks the line form from `fd_passing`.
- `src/openssh.rs`: `ResolvedConfig::proxy_use_fdpass`, parsed from `ssh -G` and carried into the route.
- `src/cli.rs`: `Interrupts::during` resets the flag and reports a late signal, and the `test` command stops before the trust prompt on one.
- Existing tests that build a `Route::Proxied` whose hostname does not match the fixture's line are updated to use the line's host.

## Testing

- **Probe (unit):** with the two-jump fixture, the target's line is found and the hop's `(via proxy)` line is rejected. With the fdpass fixture, the direct form is accepted only when `fd_passing` is set, and a `(via proxy)` line is rejected then. A `(via proxy)` line whose host matches the target in a different case is accepted.
- **Process handling (unit, fake `ssh`):** a fake that prints the two-jump fixture and hangs returns the target's success, not hop `b`'s. A fake that prints the fdpass fixture returns success for a fd-passing route instead of timing out.
- **Resolution (unit):** `proxyusefdpass yes` in `ssh -G` output sets `fd_passing` on the route.
- **CLI (unit):** `Interrupts::during` runs the next step with a cleared flag after the previous one left it set, and reports a flag set during a step that finished. A child process cannot time a real signal into that window reliably, so the command is tested through a test-only `Interrupts` that raises the flag just after a chosen step. A first test with an unknown key and a late signal prints the fingerprint and `cancelled`, exits 130, and leaves `known_hosts` untouched. A late signal after the retest still prints the retest's success and exits 0.
