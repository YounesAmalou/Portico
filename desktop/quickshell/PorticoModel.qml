import QtQuick
import Quickshell
import "portico.js" as Portico

// Runs the portico CLI for the panel and holds what it said. Nothing runs
// while the panel is closed: open() lists hosts and tunnels, the tunnels are
// listed again every few seconds while it stays open, and containers are
// listed only when a host is expanded. Actions run in their own processes;
// start and stop run under `setsid --fork --wait`, so a reload that kills the
// panel's processes leaves Portico to finish them.
Item {
    id: model
    visible: false

    // Seconds between tunnel listings while the panel is open.
    property int refreshInterval: 5

    property bool opened: false
    // Bumped on every opening, so a listing from an earlier one is dropped.
    property int session: 0

    property string program: ""
    property bool setsid: false
    // A view-wide problem ({kind, message, next}): Portico not found, or a
    // document with another schema. The view then shows only this.
    property var problem: null

    property bool hostsLoaded: false
    property var hosts: []
    property var hostWarnings: []
    property var hostsError: null

    property bool tunnelsLoaded: false
    property var tunnels: []
    property var tunnelsError: null
    property bool tunnelsRunning: false
    // Counts finished actions: a listing started before the latest one may
    // carry the old status, so it is dropped.
    property int actionsFinished: 0

    // target ("" for this machine, otherwise the alias) -> {state, rows, error}
    property var containers: ({})
    property var expanded: ({})
    // tunnel name -> "starting" | "stopping"
    property var busy: ({})
    // row key -> "opening" while connect or shell runs
    property var launching: ({})
    // row key -> {message, next}
    property var rowErrors: ({})

    signal requestClose()

    function assign(map, key, value) {
        let copy = Object.assign({}, map)
        if (value === undefined)
            delete copy[key]
        else
            copy[key] = value
        return copy
    }

    function run(command, done) {
        let r = runner.createObject(model, { command: command })
        r.finished.connect(function (stdout, stderr, exitCode) {
            done(stdout, stderr, exitCode)
            r.destroy()
        })
        r.notStarted.connect(function () {
            model.problem = Portico.notFound([model.program || command[0]])
            done(null, "", -1)
            r.destroy()
        })
        r.running = true
    }

    function open() {
        session += 1
        opened = true
        containers = ({})
        expanded = ({})
        rowErrors = ({})
        refresh()
    }

    function close() {
        opened = false
    }

    // Finds portico again (it may have been installed or moved), then lists.
    function refresh() {
        let current = session
        run(Portico.lookupCommand(Quickshell.env("PORTICO_BIN"), Quickshell.env("HOME")), function (stdout) {
            if (current !== session || stdout === null)
                return
            let found = Portico.parseLookup(stdout)
            model.setsid = found.setsid
            if (found.program === "") {
                model.program = ""
                model.problem = Portico.notFound(Portico.candidates(Quickshell.env("PORTICO_BIN"), Quickshell.env("HOME")))
                return
            }
            model.program = found.program
            model.problem = null
            listHosts()
            listTunnels()
            Object.keys(model.expanded).forEach(function (target) { model.loadContainers(target) })
        })
    }

    // A document with another schema, or none, is a view-wide problem.
    function read(stdout, exitCode) {
        let parsed = Portico.parse(stdout, exitCode)
        if (!parsed.ok) {
            model.problem = parsed.problem
            return null
        }
        return parsed.doc
    }

    function listHosts() {
        let current = session
        run([program, "hosts", "--json"], function (stdout, stderr, exitCode) {
            if (current !== session || stdout === null)
                return
            let doc = read(stdout, exitCode)
            if (!doc)
                return
            model.hostsError = Portico.documentError(doc)
            model.hosts = model.hostsError ? [] : Portico.hostRows(doc)
            model.hostWarnings = Portico.warningMessages(doc)
            model.hostsLoaded = true
        })
    }

    function listTunnels() {
        if (tunnelsRunning || program === "")
            return
        tunnelsRunning = true
        let current = session
        let before = actionsFinished
        run([program, "tunnels", "--json"], function (stdout, stderr, exitCode) {
            model.tunnelsRunning = false
            if (stdout === null)
                return
            // A listing from an earlier opening is stale: list again for this one
            // rather than wait for the next tick.
            if (current !== model.session) {
                if (model.opened)
                    model.listTunnels()
                return
            }
            if (before !== model.actionsFinished)
                return
            let doc = read(stdout, exitCode)
            if (!doc)
                return
            model.tunnelsError = Portico.documentError(doc)
            model.tunnels = model.tunnelsError ? [] : Portico.tunnelRows(doc)
            model.tunnelsLoaded = true
        })
    }

    function setExpanded(target, on) {
        expanded = assign(expanded, target, on ? true : undefined)
        if (on && Portico.needsListing(containers[target]))
            loadContainers(target)
    }

    // Lists a host's containers again (its refresh action, and refresh()).
    function reloadContainers(target) {
        loadContainers(target)
    }

    function loadContainers(target) {
        if (program === "")
            return
        let current = session
        containers = assign(containers, target, { state: "loading", rows: [], error: null })
        run(Portico.containersCommand(program, target), function (stdout, stderr, exitCode) {
            if (current !== session || stdout === null)
                return
            let parsed = Portico.parse(stdout, exitCode)
            let entry
            if (!parsed.ok)
                entry = { state: "failed", rows: [], error: parsed.problem }
            else if (Portico.documentError(parsed.doc))
                entry = { state: "failed", rows: [], error: Portico.documentError(parsed.doc) }
            else
                entry = { state: "loaded", rows: Portico.containerRows(parsed.doc, target), error: null }
            model.containers = model.assign(model.containers, target, entry)
        })
    }

    function launch(key, command) {
        if (launching[key] || program === "")
            return
        launching = assign(launching, key, "opening")
        rowErrors = assign(rowErrors, key, undefined)
        run(command, function (stdout, stderr, exitCode) {
            model.launching = model.assign(model.launching, key, undefined)
            if (stdout === null)
                return
            if (exitCode === 0)
                model.requestClose()
            else
                model.rowErrors = model.assign(model.rowErrors, key, { message: Portico.launchFailure(exitCode, stderr), next: [] })
        })
    }

    function connect(alias) {
        launch("host:" + alias, Portico.connectCommand(program, alias))
    }

    function openShell(target, id) {
        launch("container:" + target + "/" + id, Portico.shellCommand(program, target, id))
    }

    function act(verb, name) {
        if (busy[name] || program === "")
            return
        let key = "tunnel:" + name
        busy = assign(busy, name, verb === "start" ? "starting" : "stopping")
        rowErrors = assign(rowErrors, key, undefined)
        run(Portico.actionCommand(program, verb, name, setsid), function (stdout, stderr, exitCode) {
            model.actionsFinished += 1
            model.busy = model.assign(model.busy, name, undefined)
            if (stdout === null)
                return
            let result = Portico.actionOutcome(stdout, exitCode, name)
            if (result.row) {
                model.tunnels = model.tunnels.map(function (row) { return row.name === name ? result.row : row })
            }
            if (result.error)
                model.rowErrors = model.assign(model.rowErrors, key, result.error)
        })
    }

    function start(name) { act("start", name) }
    function stop(name) { act("stop", name) }

    Timer {
        interval: model.refreshInterval * 1000
        repeat: true
        running: model.opened && model.program !== "" && model.problem === null
        onTriggered: model.listTunnels()
    }

    Component {
        id: runner
        PorticoRun {}
    }
}
