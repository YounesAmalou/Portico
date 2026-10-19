.pragma library

// Reads Portico's `--json` documents (docs/json-output.md) and turns them into
// the rows the panel shows. Pure functions only: no Qt types and no I/O, so
// tests/tst_portico.qml can run them against the golden files.

// The one schema this panel reads. tests/docs.rs checks it against the golden files.
var SCHEMA = 1

var ERROR_KINDS = ["usage", "unknown-host", "unknown-tunnel", "ssh", "docker", "other"]

var OLD_PORTICO = "This Portico does not print the JSON this panel reads; it needs a Portico release with --json."

function problem(message, next) {
    return { ok: false, problem: { kind: "other", message: message, next: next || [] } }
}

// The only reader of raw stdout. Returns {ok: true, doc} for a schema 1
// document (which may still be an error document), or {ok: false, problem}.
function parse(stdout, exitCode) {
    var text = String(stdout === undefined || stdout === null ? "" : stdout)
    var doc = null
    try {
        doc = JSON.parse(text)
    } catch (e) {
        doc = null
    }
    if (doc === null || typeof doc !== "object" || Array.isArray(doc) || !("schema" in doc))
        return problem(OLD_PORTICO, exitCode === 2 ? ["Update Portico, then reopen the panel."] : [])
    var schema = doc.schema
    if (schema === SCHEMA)
        return { ok: true, doc: doc }
    if (typeof schema === "number" && schema > SCHEMA)
        return problem("This Portico prints schema " + schema + "; this panel reads schema " + SCHEMA + ".",
                       ["Update the panel."])
    return problem("This Portico prints schema " + String(schema) + "; this panel reads schema " + SCHEMA + ".",
                   ["Update Portico."])
}

function text(value) {
    return value === undefined || value === null ? "" : String(value)
}

function list(value) {
    return Array.isArray(value) ? value : []
}

// An error object as {kind, message, next, problem}; an unknown kind is `other`.
function errorInfo(error) {
    if (!error || typeof error !== "object")
        return null
    var kind = ERROR_KINDS.indexOf(error.kind) >= 0 ? error.kind : "other"
    return {
        kind: kind,
        message: text(error.message),
        next: list(error.next).map(text),
        problem: error.problem === undefined ? null : text(error.problem)
    }
}

// The error of a parsed document, or null.
function documentError(doc) {
    return doc && doc.error ? errorInfo(doc.error) : null
}

function warningMessages(doc) {
    return list(doc && doc.warnings).map(function (w) { return text(w && w.message) })
                                    .filter(function (m) { return m !== "" })
}

// Hosts in the document's order; a duplicate alias is shown once, the first,
// as ssh uses the first match.
function hostRows(doc) {
    var seen = {}
    var rows = []
    list(doc && doc.hosts).forEach(function (host) {
        var alias = text(host && host.alias)
        if (alias === "" || seen[alias])
            return
        seen[alias] = true
        rows.push({ key: "host:" + alias, alias: alias, external: host.ownership === "external" })
    })
    return rows
}

var STATES = {
    "running": { text: "Running", action: "stop" },
    "stopped": { text: "Stopped", action: "start" },
    "failed": { text: "Failed", action: "start" },
    "port-in-use": { text: "Port in use", action: "start" }
}

function tunnelRow(report) {
    var name = text(report && report.name)
    var status = (report && report.status) || {}
    var forward = (report && report.forward) || {}
    var state = text(status.state)
    var known = STATES[state]
    var detail = ""
    if (state === "failed") {
        detail = text(status.reason)
    } else if (state === "port-in-use") {
        detail = status.by ? "held by tunnel " + text(status.by)
                           : "another program listens on " + text(status.address)
    }
    return {
        key: "tunnel:" + name,
        name: name,
        host: text(report && report.host),
        description: text(forward.description),
        state: state,
        stateText: known ? known.text : (state || "Unknown"),
        detail: detail,
        action: known ? known.action : null,
        warnings: warningMessages(report)
    }
}

function tunnelRows(doc) {
    return list(doc && doc.tunnels).map(tunnelRow)
}

// `target` is "" for this machine, otherwise the alias.
function containerRows(doc, target) {
    return list(doc && doc.containers).map(function (c) {
        c = c || {}
        var id = text(c.id)
        return {
            key: "container:" + target + "/" + id,
            target: target,
            id: id,
            name: text(c.display_name) || text(c.name) || id,
            dockerName: text(c.name),
            image: text(c.image),
            status: text(c.status)
        }
    })
}

// What a `tunnel start|stop --json` document says about `name`: the row to
// show (null when the tunnel is unknown), the outcome and the error, if any.
function actionResult(doc, name) {
    var results = list(doc && doc.results)
    var result = null
    for (var i = 0; i < results.length; i++) {
        if (results[i] && results[i].name === name) {
            result = results[i]
            break
        }
    }
    if (!result) {
        var error = documentError(doc)
        return { row: null, outcome: "failed", error: error }
    }
    return {
        row: result.tunnel ? tunnelRow(result.tunnel) : null,
        outcome: text(result.outcome),
        error: result.outcome === "failed" ? (errorInfo(result.error) || { kind: "other", message: "Portico reported a failure.", next: [], problem: null }) : null
    }
}

// Lines to show for an error: the message, then each next step.
function errorLines(error) {
    if (!error)
        return []
    return [error.message].concat(error.next).filter(function (l) { return l !== "" })
}

// Case-insensitive substring match on any of `fields`; an empty text keeps all.
function matches(row, query, fields) {
    var q = text(query).trim().toLowerCase()
    if (q === "")
        return true
    for (var i = 0; i < fields.length; i++) {
        if (text(row[fields[i]]).toLowerCase().indexOf(q) >= 0)
            return true
    }
    return false
}

function filterRows(rows, query, fields) {
    return list(rows).filter(function (row) { return matches(row, query, fields) })
}

// The fields a container row is searched on.
var CONTAINER_FIELDS = ["name", "dockerName", "image", "id"]

// How a host shows under the filter. `hostMatches` says whether the host's own
// name matches; `entry` is its containers entry (absent until listed). A host
// that matches shows as usual: its containers when expanded. A host that does
// not match is kept when its loaded containers match, showing only those, even
// when collapsed, so a container can be found by name; otherwise it is hidden.
// containers is null when no container rows are to be shown (collapsed, or the
// entry is loading or failed and drawn by the caller).
function hostView(hostMatches, entry, query, expanded) {
    var loaded = entry && entry.state === "loaded" ? list(entry.rows) : null
    if (text(query).trim() === "" || hostMatches)
        return { show: true, containers: expanded && loaded ? loaded : null, filtered: false }
    var found = loaded ? filterRows(loaded, query, CONTAINER_FIELDS) : []
    if (found.length === 0)
        return { show: false, containers: null, filtered: false }
    return { show: true, containers: found, filtered: true }
}

// The last non-empty line of human text, as one plain line.
function lastLine(output) {
    var lines = text(output).split("\n")
    for (var i = lines.length - 1; i >= 0; i--) {
        var line = lines[i].replace(/\u001b\[[0-9;?]*[A-Za-z]/g, "").replace(/[\u0000-\u001f\u007f]/g, "").trim()
        if (line !== "")
            return line.length > 300 ? line.slice(0, 300) + "…" : line
    }
    return ""
}

// Why `connect` or `shell --new-terminal` failed, from its exit status and stderr.
function launchFailure(exitCode, stderr) {
    var line = lastLine(stderr)
    return line !== "" ? line : "Portico could not open a terminal (exit status " + exitCode + ")."
}

// The view-wide problem when no program was found.
function notFound(candidates) {
    return {
        kind: "not-found",
        message: "Portico was not found.",
        next: ["Looked for: " + candidates.join(", ") + ".",
               "Set PORTICO_BIN to its full path (in Hyprland: env = PORTICO_BIN,/path/to/portico), then press Refresh."]
    }
}

// The lookup script's output: `portico=<path>` and `setsid=yes` lines.
function parseLookup(output) {
    var found = { program: "", setsid: false }
    text(output).split("\n").forEach(function (line) {
        if (line.indexOf("portico=") === 0)
            found.program = line.slice("portico=".length)
        else if (line === "setsid=yes")
            found.setsid = true
    })
    return found
}

// The candidates the lookup tries, in order: $PORTICO_BIN alone when set,
// otherwise `portico` on PATH and then cargo's install directory.
function candidates(porticoBin, home) {
    if (text(porticoBin) !== "")
        return [text(porticoBin)]
    var list = ["portico"]
    if (text(home) !== "")
        list.push(text(home) + "/.cargo/bin/portico")
    return list
}

// The banner shown when `portico desktop install` wrote this panel for another
// Portico: `stamp` is the VERSION file next to the panel ("0.6.0+<hash>"), and
// `versionOutput` what `portico --version` printed. A panel run from a checkout
// has no stamp, and gets no banner.
function versionBanner(stamp, versionOutput) {
    var panel = text(stamp).trim().split("+")[0]
    var match = /^portico\s+(\S+)$/.exec(lastLine(versionOutput))
    if (panel === "" || !match || match[1] === panel)
        return ""
    return "This panel is from Portico " + panel + " and portico is " + match[1] +
        ". Run portico desktop install to update it."
}

// A local file path from a file: URL (Qt.resolvedUrl of a file next to the panel).
function localPath(url) {
    var value = text(url)
    return value.indexOf("file://") === 0 ? decodeURIComponent(value.slice("file://".length)) : value
}

// Positional arguments only: the candidates are never part of the script.
var LOOKUP_SCRIPT = 'for c in "$@"; do p=$(command -v "$c") && [ -x "$p" ] && { printf "portico=%s\\n" "$p"; break; }; done; command -v setsid >/dev/null 2>&1 && printf "setsid=yes\\n"; exit 0'

function lookupCommand(porticoBin, home) {
    return ["sh", "-c", LOOKUP_SCRIPT, "portico-lookup"].concat(candidates(porticoBin, home))
}

// Whether a host's containers need listing when it is expanded: never listed,
// or the last listing failed (docker may have been started since).
function needsListing(entry) {
    return !entry || entry.state === "failed"
}

// The commands the panel runs. `--on=<alias>` and `--` keep an alias, ID or
// name that starts with "-" from being read as a flag; tests/cli.rs,
// tests/containers.rs and tests/json.rs pin each form.
function onArgument(target) {
    return text(target) === "" ? [] : ["--on=" + text(target)]
}

function containersCommand(program, target) {
    return [program, "containers", "--json"].concat(onArgument(target))
}

function connectCommand(program, alias) {
    return [program, "connect", "--new-terminal", "--", alias]
}

function shellCommand(program, target, id) {
    return [program, "shell", "--new-terminal"].concat(onArgument(target), ["--", id])
}

// Start and stop run under `setsid --fork --wait` when it exists, so a reload
// that kills the panel's processes leaves Portico to finish them.
function actionCommand(program, verb, name, setsid) {
    var command = [program, "tunnel", verb, "--json", "--", name]
    return setsid ? ["setsid", "--fork", "--wait"].concat(command) : command
}

// What a start or stop says about its row: a document that is not schema 1
// is that row's error, so the rest of the panel keeps working.
function actionOutcome(stdout, exitCode, name) {
    var parsed = parse(stdout, exitCode)
    if (!parsed.ok)
        return { row: null, outcome: "failed", error: parsed.problem }
    return actionResult(parsed.doc, name)
}
