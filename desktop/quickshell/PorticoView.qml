pragma ComponentBehavior: Bound
import QtQuick
import "portico.js" as Portico

// The panel's content, shared by every entry point: the saved tunnels with a
// start or stop action, then the hosts (and this machine), each expandable to
// its running containers. Keyboard first: typing filters, Up and Down move,
// Enter acts, Right expands, Left collapses, Escape clears or closes, F5 and
// Ctrl+R list again. Every text from Portico is drawn as plain text.
FocusScope {
    id: view

    required property var model
    // Colours and font; each entry point fills these from its theme.
    property var theme: ({})
    property int maxListHeight: 460

    readonly property color fg: theme.foreground || "#d8dadc"
    readonly property color dim: theme.dim || Qt.darker(fg, 1.6)
    readonly property color accent: theme.accent || "#8fb8de"
    readonly property color urgent: theme.urgent || "#e08080"
    readonly property color highlight: theme.highlight || Qt.rgba(fg.r, fg.g, fg.b, 0.10)
    readonly property color border: theme.border || Qt.rgba(fg.r, fg.g, fg.b, 0.25)
    readonly property string fontFamily: theme.fontFamily || "monospace"
    readonly property int fontSize: theme.fontSize || 13

    property string currentKey: ""
    readonly property var items: buildItems()

    implicitWidth: 440
    implicitHeight: content.implicitHeight

    function takeFocus() {
        filter.forceActiveFocus()
    }

    function hint(text) {
        return { type: "hint", lines: [text] }
    }

    function message(error) {
        return { type: "message", lines: Portico.errorLines(error) }
    }

    function containerItems(target) {
        let entry = model.containers[target]
        if (!entry || entry.state === "loading")
            return [hint("Listing containers…")]
        if (entry.state === "failed")
            return [message(entry.error)]
        if (entry.rows.length === 0)
            return [hint("No running containers.")]
        return entry.rows.map(function (row) {
            return { type: "container", key: row.key, row: row, target: target,
                     busy: !!model.launching[row.key], error: model.rowErrors[row.key] || null }
        })
    }

    function hostItem(key, target, title, external) {
        let open = !!model.expanded[target]
        let item = { type: "host", key: key, target: target, title: title, external: external,
                     local: target === "" && key === "local:", expanded: open,
                     busy: !!model.launching[key], error: model.rowErrors[key] || null }
        return [item].concat(open ? containerItems(target) : [])
    }

    function buildItems() {
        if (model.problem)
            return [message(model.problem)]
        let query = filter.text
        let list = [{ type: "section", text: "Tunnels" }]
        if (model.tunnelsError) {
            list.push(message(model.tunnelsError))
        } else if (!model.tunnelsLoaded) {
            list.push(hint("Reading tunnels…"))
        } else {
            let tunnels = Portico.filterRows(model.tunnels, query, ["name", "host"])
            if (tunnels.length === 0)
                list.push(hint(model.tunnels.length === 0 ? "No saved tunnels: add one with portico tunnel add." : "No tunnel matches."))
            tunnels.forEach(function (row) {
                list.push({ type: "tunnel", key: row.key, row: row, busy: model.busy[row.name] || "",
                            error: model.rowErrors[row.key] || null })
            })
        }

        list.push({ type: "section", text: "Hosts" })
        if (Portico.matches({ name: "this machine local" }, query, ["name"]))
            list = list.concat(hostItem("local:", "", "This machine", false))
        if (model.hostsError) {
            list.push(message(model.hostsError))
        } else if (!model.hostsLoaded) {
            list.push(hint("Reading hosts…"))
        } else {
            let hosts = Portico.filterRows(model.hosts, query, ["alias"])
            if (hosts.length === 0)
                list.push(hint(model.hosts.length === 0 ? "No hosts: add one with portico add or the TUI." : "No host matches."))
            hosts.forEach(function (row) {
                list = list.concat(hostItem(row.key, row.alias, row.alias, row.external))
            })
            model.hostWarnings.forEach(function (warning) { list.push(hint("Warning: " + warning)) })
        }
        return list
    }

    function selectable(item) {
        return item.type === "tunnel" || item.type === "host" || item.type === "container"
    }

    function indexOfKey(key) {
        for (let i = 0; i < items.length; i++)
            if (items[i].key === key && selectable(items[i]))
                return i
        return -1
    }

    readonly property int currentIndex: {
        let i = indexOfKey(currentKey)
        if (i >= 0)
            return i
        for (let j = 0; j < items.length; j++)
            if (selectable(items[j]))
                return j
        return -1
    }

    function move(step) {
        let i = currentIndex
        for (let j = i + step; j >= 0 && j < items.length; j += step) {
            if (selectable(items[j])) {
                currentKey = items[j].key
                listView.positionViewAtIndex(j, ListView.Contain)
                return
            }
        }
    }

    function activate(item) {
        if (!item)
            return
        currentKey = item.key
        if (item.type === "tunnel") {
            if (item.busy || !item.row.action)
                return
            if (item.row.action === "stop")
                model.stop(item.row.name)
            else
                model.start(item.row.name)
        } else if (item.type === "host") {
            if (item.local)
                model.setExpanded("", !item.expanded)
            else
                model.connect(item.target)
        } else if (item.type === "container") {
            model.openShell(item.target, item.row.id)
        }
    }

    function expand(item, on) {
        if (!item)
            return
        if (item.type === "host") {
            model.setExpanded(item.target, on)
        } else if (item.type === "container" && !on) {
            let host = item.target === "" ? "local:" : "host:" + item.target
            model.setExpanded(item.target, false)
            currentKey = host
        }
    }

    function current() {
        return currentIndex >= 0 ? items[currentIndex] : null
    }

    Column {
        id: content
        width: parent.width
        spacing: 8

        Item {
            width: parent.width
            height: title.implicitHeight

            Text {
                id: title
                text: "Portico"
                color: view.fg
                font.family: view.fontFamily
                font.pixelSize: view.fontSize + 2
                font.bold: true
            }

            Text {
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                text: "↻ Refresh"
                color: refreshArea.containsMouse ? view.fg : view.dim
                font.family: view.fontFamily
                font.pixelSize: view.fontSize - 1
                MouseArea {
                    id: refreshArea
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    onClicked: view.model.refresh()
                }
            }
        }

        Rectangle {
            width: parent.width
            height: filter.implicitHeight + 12
            radius: 4
            color: "transparent"
            border.color: filter.activeFocus ? view.accent : view.border
            border.width: 1

            TextInput {
                id: filter
                anchors.fill: parent
                anchors.margins: 6
                focus: true
                clip: true
                color: view.fg
                selectionColor: view.accent
                font.family: view.fontFamily
                font.pixelSize: view.fontSize
                verticalAlignment: TextInput.AlignVCenter

                Text {
                    anchors.fill: parent
                    verticalAlignment: Text.AlignVCenter
                    visible: filter.text === ""
                    text: "Filter tunnels and hosts"
                    color: view.dim
                    font: filter.font
                }

                Keys.onPressed: function (event) {
                    let ctrl = (event.modifiers & Qt.ControlModifier) !== 0
                    if (event.key === Qt.Key_Down || (ctrl && event.key === Qt.Key_N)) {
                        view.move(1)
                    } else if (event.key === Qt.Key_Up || (ctrl && event.key === Qt.Key_P)) {
                        view.move(-1)
                    } else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
                        view.activate(view.current())
                    } else if (event.key === Qt.Key_Right && filter.cursorPosition === filter.text.length) {
                        view.expand(view.current(), true)
                    } else if (event.key === Qt.Key_Left && filter.text === "") {
                        view.expand(view.current(), false)
                    } else if (event.key === Qt.Key_Escape) {
                        if (filter.text !== "")
                            filter.text = ""
                        else
                            view.model.requestClose()
                    } else if (event.key === Qt.Key_F5 || (ctrl && event.key === Qt.Key_R)) {
                        view.model.refresh()
                    } else {
                        return
                    }
                    event.accepted = true
                }
            }
        }

        ListView {
            id: listView
            width: parent.width
            height: Math.min(contentHeight, view.maxListHeight)
            clip: true
            interactive: contentHeight > height
            boundsBehavior: Flickable.StopAtBounds
            model: view.items

            delegate: Item {
                id: row
                required property var modelData
                required property int index
                readonly property var item: modelData
                readonly property bool isCurrent: index === view.currentIndex
                readonly property bool isRow: view.selectable(item)

                width: ListView.view.width
                height: body.implicitHeight + (item.type === "section" ? 6 : 8)

                Rectangle {
                    anchors.fill: parent
                    radius: 4
                    color: row.isRow && (row.isCurrent || hover.containsMouse) ? view.highlight : "transparent"
                }

                MouseArea {
                    id: hover
                    anchors.fill: parent
                    enabled: row.isRow
                    hoverEnabled: true
                    cursorShape: row.isRow ? Qt.PointingHandCursor : Qt.ArrowCursor
                    onClicked: view.activate(row.item)
                }

                Column {
                    id: body
                    x: row.item.type === "container" ? 26 : 8
                    y: row.item.type === "section" ? 6 : 4
                    width: parent.width - x - 8
                    spacing: 2

                    // Section titles, hints and messages.
                    Repeater {
                        model: row.item.type === "section" ? [row.item.text] : (row.item.lines || [])
                        Text {
                            required property var modelData
                            required property int index
                            width: body.width
                            text: modelData
                            textFormat: Text.PlainText
                            wrapMode: Text.Wrap
                            color: row.item.type === "message" && index === 0 ? view.urgent : view.dim
                            font.family: view.fontFamily
                            font.pixelSize: row.item.type === "section" ? view.fontSize - 2 : view.fontSize - 1
                            font.bold: row.item.type === "section"
                            font.capitalization: row.item.type === "section" ? Font.AllUppercase : Font.MixedCase
                        }
                    }

                    // The first line of a row: a marker, its name, and its state or action.
                    Item {
                        visible: row.isRow
                        width: body.width
                        height: name.implicitHeight

                        Text {
                            id: marker
                            width: 16
                            text: row.item.type === "tunnel" ? "●"
                                : row.item.type === "host" ? (row.item.expanded ? "▾" : "▸")
                                : "›"
                            textFormat: Text.PlainText
                            color: row.item.type === "tunnel"
                                   ? (row.item.row.state === "running" ? view.accent
                                      : row.item.row.state === "stopped" ? view.dim : view.urgent)
                                   : view.dim
                            font.family: view.fontFamily
                            font.pixelSize: view.fontSize
                            MouseArea {
                                anchors.fill: parent
                                anchors.margins: -4
                                enabled: row.item.type === "host"
                                cursorShape: Qt.PointingHandCursor
                                onClicked: {
                                    view.currentKey = row.item.key
                                    view.model.setExpanded(row.item.target, !row.item.expanded)
                                }
                            }
                        }

                        Text {
                            id: name
                            anchors.left: marker.right
                            anchors.right: side.left
                            anchors.rightMargin: 8
                            elide: Text.ElideRight
                            textFormat: Text.PlainText
                            text: row.item.type === "tunnel" ? row.item.row.name
                                : row.item.type === "host" ? row.item.title
                                : row.isRow ? row.item.row.name : ""
                            color: view.fg
                            font.family: view.fontFamily
                            font.pixelSize: view.fontSize
                        }

                        Row {
                            id: side
                            anchors.right: parent.right
                            spacing: 8

                            Text {
                                anchors.verticalCenter: parent.verticalCenter
                                textFormat: Text.PlainText
                                text: row.item.type === "tunnel"
                                      ? (row.item.busy === "starting" ? "Starting…"
                                         : row.item.busy === "stopping" ? "Stopping…" : row.item.row.stateText)
                                      : row.item.busy ? "Opening…"
                                      : row.item.type === "host" && row.item.external ? "external"
                                      : row.item.type === "container" ? row.item.row.status : ""
                                color: view.dim
                                font.family: view.fontFamily
                                font.pixelSize: view.fontSize - 1
                            }

                            Rectangle {
                                visible: row.item.type === "tunnel" && !!row.item.row.action
                                anchors.verticalCenter: parent.verticalCenter
                                width: actionText.implicitWidth + 14
                                height: actionText.implicitHeight + 4
                                radius: 3
                                color: "transparent"
                                border.width: 1
                                border.color: actionArea.containsMouse && !row.item.busy ? view.accent : view.border
                                opacity: row.item.busy ? 0.4 : 1

                                Text {
                                    id: actionText
                                    anchors.centerIn: parent
                                    text: row.item.type === "tunnel" && row.item.row.action === "stop" ? "Stop" : "Start"
                                    color: view.fg
                                    font.family: view.fontFamily
                                    font.pixelSize: view.fontSize - 1
                                }

                                MouseArea {
                                    id: actionArea
                                    anchors.fill: parent
                                    hoverEnabled: true
                                    enabled: !row.item.busy
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: view.activate(row.item)
                                }
                            }
                        }
                    }

                    // Further lines: what the tunnel forwards, its detail and
                    // warnings, a container's image, and the row's error.
                    Repeater {
                        model: {
                            let it = row.item
                            let lines = []
                            if (it.type === "tunnel") {
                                lines.push({ text: it.row.description + "  via " + it.row.host, tone: "dim" })
                                if (it.row.detail !== "")
                                    lines.push({ text: it.row.detail, tone: it.row.state === "failed" || it.row.state === "port-in-use" ? "urgent" : "dim" })
                                it.row.warnings.forEach(function (w) { lines.push({ text: w, tone: "dim" }) })
                            } else if (it.type === "container") {
                                lines.push({ text: it.row.image, tone: "dim" })
                            }
                            if (it.error)
                                Portico.errorLines(it.error).forEach(function (l, i) {
                                    if (it.type !== "tunnel" || l !== it.row.detail)
                                        lines.push({ text: l, tone: i === 0 ? "urgent" : "dim" })
                                })
                            return lines
                        }
                        Text {
                            required property var modelData
                            x: 16
                            width: body.width - 16
                            text: modelData.text
                            textFormat: Text.PlainText
                            wrapMode: Text.Wrap
                            color: modelData.tone === "urgent" ? view.urgent : view.dim
                            font.family: view.fontFamily
                            font.pixelSize: view.fontSize - 1
                        }
                    }
                }
            }
        }

        Text {
            width: parent.width
            text: "Enter connects or starts · → containers · Esc closes · manage hosts and tunnels in portico"
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            color: view.dim
            font.family: view.fontFamily
            font.pixelSize: view.fontSize - 3
        }
    }
}
