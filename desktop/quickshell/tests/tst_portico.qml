import QtQuick
import QtTest
import "../portico.js" as Portico

// Runs portico.js in Qt's JavaScript engine, the one Quickshell uses, against
// Portico's golden files (tests/fixtures/json). scripts/ci-checks.sh desktop
// runs it with QML_XHR_ALLOW_FILE_READ=1. tests/docs.rs checks that every
// golden file is named here.
TestCase {
    name: "Portico"

    function golden(file) {
        var request = new XMLHttpRequest()
        request.open("GET", Qt.resolvedUrl("../../../tests/fixtures/json/" + file), false)
        request.send()
        verify(request.responseText.length > 0, "golden file " + file + " is readable")
        return request.responseText
    }

    function doc(file) {
        var parsed = Portico.parse(golden(file), 0)
        verify(parsed.ok, file + " parses with schema " + Portico.SCHEMA)
        return parsed.doc
    }

    function test_every_golden_file_has_the_panel_schema() {
        var files = ["containers-docker-down.json", "containers.json", "containers-on-host.json",
                     "error-unknown-host.json", "error-usage.json", "hosts.json", "identities.json",
                     "show.json", "show-unresolved.json", "tunnel-show.json", "tunnels.json",
                     "tunnel-start.json", "tunnel-stop-all.json"]
        files.forEach(function (file) { doc(file) })
    }

    function test_hosts_are_deduplicated_with_their_warning() {
        var d = doc("hosts.json")
        var rows = Portico.hostRows(d)
        compare(rows.map(function (r) { return r.alias }), ["bastion", "legacy", "prod"])
        compare(rows[0].external, true)
        compare(rows[2].external, false)
        compare(rows[2].key, "host:prod")
        compare(Portico.warningMessages(d), ["duplicate alias bastion: /home/me/.ssh/config, /home/me/.ssh/config"])
        compare(Portico.documentError(d), null)
    }

    function test_tunnels_have_a_state_and_an_action() {
        var rows = Portico.tunnelRows(doc("tunnels.json"))
        compare(rows.length, 4)

        compare(rows[0].name, "busy")
        compare(rows[0].stateText, "Port in use")
        compare(rows[0].detail, "another program listens on 127.0.0.1:5433")
        compare(rows[0].action, "start")
        compare(rows[0].host, "prod")
        compare(rows[0].description, "127.0.0.1:5433 → localhost:80")

        compare(rows[1].name, "db")
        compare(rows[1].stateText, "Stopped")
        compare(rows[1].action, "start")
        compare(rows[1].warnings, ["uses the same port as tunnel socks; only one of them can run at a time"])

        compare(rows[2].name, "preview")
        compare(rows[2].stateText, "Failed")
        compare(rows[2].detail, "Error: remote port forwarding failed for listen port 8080")
        compare(rows[2].action, "start")

        compare(rows[3].description, "socks 127.0.0.1:1")
    }

    function test_tunnel_show_report_is_a_row() {
        var row = Portico.tunnelRow(doc("tunnel-show.json").tunnel)
        compare(row.name, "preview")
        compare(row.state, "failed")
    }

    function test_a_running_tunnel_is_stopped_and_a_holder_is_named() {
        var running = Portico.tunnelRow({ name: "db", host: "prod", forward: { description: "x" },
                                          status: { state: "running", pid: 42, started: 1 }, warnings: [] })
        compare(running.stateText, "Running")
        compare(running.action, "stop")
        var held = Portico.tunnelRow({ name: "a", status: { state: "port-in-use", address: "127.0.0.1:1", by: "b" } })
        compare(held.detail, "held by tunnel b")
    }

    function test_an_unknown_state_offers_no_action() {
        var row = Portico.tunnelRow({ name: "x", status: { state: "paused" } })
        compare(row.stateText, "paused")
        compare(row.action, null)
    }

    function test_containers_on_a_host_and_here() {
        var host = Portico.containerRows(doc("containers-on-host.json"), "prod")
        compare(host.length, 2)
        compare(host[0].id, "4f1c2a9b7d3e")
        compare(host[0].name, "web")
        compare(host[0].image, "nginx:1.27")
        compare(host[0].status, "Up 3 hours")
        compare(host[0].key, "container:prod/4f1c2a9b7d3e")
        compare(host[1].name, "cache")
        var here = Portico.containerRows(doc("containers.json"), "")
        compare(here[0].key, "container:/4f1c2a9b7d3e")
    }

    function test_a_docker_failure_keeps_its_steps() {
        var error = Portico.documentError(doc("containers-docker-down.json"))
        compare(error.kind, "docker")
        compare(error.problem, "daemon-down")
        compare(Portico.errorLines(error), ["The Docker daemon is not running on this machine.",
                                            "Start it with: sudo systemctl start docker",
                                            "Run portico containers in a terminal to be offered this fix."])
    }

    function test_error_documents() {
        compare(Portico.documentError(doc("error-usage.json")).message, "give the name: portico show <alias> --json")
        var unknown = Portico.documentError(doc("error-unknown-host.json"))
        compare(unknown.kind, "unknown-host")
        compare(Portico.errorLines(unknown), ["unknown host: nope"])
    }

    function test_start_results() {
        var d = doc("tunnel-start.json")
        var preview = Portico.actionResult(d, "preview")
        compare(preview.outcome, "failed")
        compare(preview.row.stateText, "Failed")
        compare(preview.error.kind, "ssh")
        compare(Portico.errorLines(preview.error), ["the server refused the remote forward on port 8080",
                                                    "the port is in use on the server, or its sshd does not allow remote forwards"])
        var nope = Portico.actionResult(d, "nope")
        compare(nope.row, null)
        compare(nope.error.kind, "unknown-tunnel")
    }

    function test_stop_all_reports_nothing() {
        compare(doc("tunnel-stop-all.json").results, [])
        var missing = Portico.actionResult(doc("tunnel-stop-all.json"), "db")
        compare(missing.outcome, "failed")
        compare(missing.error, null)
    }

    function test_a_started_tunnel_has_no_error() {
        var result = Portico.actionResult({ schema: 1, results: [{ name: "db", outcome: "started", pid: 7, error: null,
            tunnel: { name: "db", status: { state: "running", pid: 7, started: 1 } } }] }, "db")
        compare(result.error, null)
        compare(result.row.action, "stop")
    }

    function test_reads_without_details_do_not_throw() {
        doc("show.json")
        doc("show-unresolved.json")
        doc("identities.json")
        compare(Portico.hostRows({ schema: 1 }), [])
        compare(Portico.tunnelRows({ schema: 1, tunnels: [{}] })[0].stateText, "Unknown")
        compare(Portico.containerRows({ schema: 1, containers: [{ id: "abcd" }] }, "")[0].name, "abcd")
    }

    function test_schema_problems() {
        var newer = Portico.parse('{"schema": 2, "hosts": []}', 0)
        compare(newer.ok, false)
        compare(newer.problem.message, "This Portico prints schema 2; this panel reads schema 1.")
        compare(newer.problem.next, ["Update the panel."])

        var older = Portico.parse('{"schema": 0}', 0)
        compare(older.problem.next, ["Update Portico."])

        var missing = Portico.parse('{"hosts": []}', 0)
        compare(missing.problem.message, Portico.OLD_PORTICO)

        compare(Portico.parse("not json", 1).ok, false)
        compare(Portico.parse("", 0).ok, false)
        compare(Portico.parse("[1]", 0).ok, false)
        var flag = Portico.parse("", 2)
        compare(flag.problem.message, Portico.OLD_PORTICO)
        compare(flag.problem.next, ["Update Portico, then reopen the panel."])
    }

    function test_unknown_kinds_are_other() {
        var parsed = Portico.parse('{"schema": 1, "error": {"kind": "quota", "message": "m", "next": ["n"], "extra": 1}}', 1)
        verify(parsed.ok)
        var error = Portico.documentError(parsed.doc)
        compare(error.kind, "other")
        compare(Portico.errorLines(error), ["m", "n"])
        compare(Portico.tunnelRow({ name: "t", status: { state: "stopped" },
                                    warnings: [{ kind: "new-kind", message: "w" }] }).warnings, ["w"])
    }

    function test_filter() {
        var rows = [{ alias: "prod-db" }, { alias: "Staging" }, { alias: "dev" }]
        compare(Portico.filterRows(rows, "", ["alias"]).length, 3)
        compare(Portico.filterRows(rows, "STAG", ["alias"]), [{ alias: "Staging" }])
        compare(Portico.filterRows(rows, " d ", ["alias"]).length, 2)
        compare(Portico.filterRows([{ name: "db", host: "prod" }], "prod", ["name", "host"]).length, 1)
    }

    function test_filter_finds_loaded_containers() {
        var rows = Portico.containerRows(doc("containers-on-host.json"), "prod")
        compare(rows[0].dockerName, "shop-web-1")
        var loaded = { state: "loaded", rows: rows }
        // No query: as before, containers only when expanded.
        compare(Portico.hostView(false, loaded, "", false), { show: true, containers: null, filtered: false })
        compare(Portico.hostView(false, loaded, "", true).containers.length, 2)
        // The host matches: shown as usual, all its containers when expanded.
        compare(Portico.hostView(true, loaded, "prod", true).containers.length, 2)
        compare(Portico.hostView(true, loaded, "prod", false).containers, null)
        // The host does not match, a loaded container does: only that one, even collapsed.
        var byName = Portico.hostView(false, loaded, "CACHE", false)
        compare(byName.show, true)
        compare(byName.filtered, true)
        compare(byName.containers.length, 1)
        compare(byName.containers[0].id, "9a8b7c6d5e4f")
        compare(Portico.hostView(false, loaded, "shop-web", false).containers[0].name, "web")
        compare(Portico.hostView(false, loaded, "redis", false).containers[0].name, "cache")
        compare(Portico.hostView(false, loaded, "4f1c2a", false).containers.length, 1)
        // Nothing matches, or the containers are not loaded: hidden.
        compare(Portico.hostView(false, loaded, "nope", true).show, false)
        compare(Portico.hostView(false, undefined, "web", false).show, false)
        compare(Portico.hostView(false, { state: "loading" }, "web", true).show, false)
        compare(Portico.hostView(false, { state: "failed", error: {} }, "web", true).show, false)
    }

    function test_launch_failure() {
        compare(Portico.launchFailure(1, "error: unknown host: nope\n\n"), "error: unknown host: nope")
        compare(Portico.launchFailure(3, ""), "Portico could not open a terminal (exit status 3).")
        compare(Portico.lastLine("a\u001b[31mred\u001b[0m\n"), "ared")
    }

    function test_lookup() {
        compare(Portico.candidates("", "/home/me"), ["portico", "/home/me/.cargo/bin/portico"])
        compare(Portico.candidates("/opt/portico", "/home/me"), ["/opt/portico"])
        var command = Portico.lookupCommand("", "/home/me")
        compare(command.slice(0, 2), ["sh", "-c"])
        compare(command.slice(3), ["portico-lookup", "portico", "/home/me/.cargo/bin/portico"])
        verify(command[2].indexOf("/home/me") < 0, "the candidates are not in the script")
        compare(Portico.parseLookup("portico=/usr/bin/portico\nsetsid=yes\n"), { program: "/usr/bin/portico", setsid: true })
        compare(Portico.parseLookup(""), { program: "", setsid: false })
        compare(Portico.notFound(["portico"]).message, "Portico was not found.")
    }

    function test_a_failed_listing_is_listed_again() {
        verify(Portico.needsListing(undefined))
        verify(Portico.needsListing({ state: "failed", rows: [], error: { message: "down" } }))
        verify(!Portico.needsListing({ state: "loading", rows: [], error: null }))
        verify(!Portico.needsListing({ state: "loaded", rows: [], error: null }))
    }

    function test_commands_keep_values_from_being_read_as_flags() {
        compare(Portico.containersCommand("/p", ""), ["/p", "containers", "--json"])
        compare(Portico.containersCommand("/p", "-dash"), ["/p", "containers", "--json", "--on=-dash"])
        compare(Portico.connectCommand("/p", "-dash"), ["/p", "connect", "--new-terminal", "--", "-dash"])
        compare(Portico.shellCommand("/p", "", "aaaa1111"), ["/p", "shell", "--new-terminal", "--", "aaaa1111"])
        compare(Portico.shellCommand("/p", "dev", "aaaa1111"), ["/p", "shell", "--new-terminal", "--on=dev", "--", "aaaa1111"])
        compare(Portico.actionCommand("/p", "start", "db", false), ["/p", "tunnel", "start", "--json", "--", "db"])
        compare(Portico.actionCommand("/p", "stop", "db", true),
                ["setsid", "--fork", "--wait", "/p", "tunnel", "stop", "--json", "--", "db"])
    }

    function test_a_start_without_json_is_the_row_error() {
        var outcome = Portico.actionOutcome("", 2, "db")
        compare(outcome.row, null)
        compare(outcome.outcome, "failed")
        compare(outcome.error.message, Portico.OLD_PORTICO)
        var started = Portico.actionOutcome(golden("tunnel-start.json"), 1, "preview")
        compare(started.error.kind, "ssh")
    }

    function test_malformed_lists_do_not_throw() {
        compare(Portico.containerRows({ schema: 1, containers: [null, { id: "abcd" }] }, "").length, 2)
        compare(Portico.hostRows({ schema: 1, hosts: [null, { alias: "a" }] }).length, 1)
        compare(Portico.tunnelRows({ schema: 1, tunnels: [null] })[0].stateText, "Unknown")
        compare(Portico.actionResult({ schema: 1, results: [null] }, "db").outcome, "failed")
    }

    function test_version_banner() {
        compare(Portico.versionBanner("0.6.0+0123456789abcdef\n", "portico 0.6.0\n"), "")
        compare(Portico.versionBanner("0.5.1+0123456789abcdef\n", "portico 0.6.0\n"),
                "This panel is from Portico 0.5.1 and portico is 0.6.0. Run portico desktop install to update it.")
        compare(Portico.versionBanner("", "portico 0.6.0\n"), "")
        compare(Portico.versionBanner(undefined, "portico 0.6.0"), "")
        compare(Portico.versionBanner("0.5.1+abc", ""), "")
        compare(Portico.versionBanner("0.5.1+abc", "something else"), "")
        compare(Portico.localPath("file:///home/me/my%20panel/VERSION"), "/home/me/my panel/VERSION")
        compare(Portico.localPath("/plain"), "/plain")
    }
}
