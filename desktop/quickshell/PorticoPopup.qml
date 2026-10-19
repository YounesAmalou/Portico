import QtQuick
import Quickshell
import Quickshell.Wayland

// The launcher-style popup: a full-screen overlay layer with a dimmed
// backdrop and the view in a centred card. Clicking the backdrop or pressing
// Escape asks to close. Used by shell.qml and OmarchyPanel.qml.
PanelWindow {
    id: popup

    required property var model
    property var theme: ({})
    property bool opened: false

    visible: opened
    color: "transparent"
    anchors { top: true; bottom: true; left: true; right: true }
    exclusionMode: ExclusionMode.Ignore
    WlrLayershell.namespace: "portico-panel"
    WlrLayershell.layer: WlrLayer.Overlay
    WlrLayershell.keyboardFocus: opened ? WlrKeyboardFocus.Exclusive : WlrKeyboardFocus.None

    onOpenedChanged: if (opened) Qt.callLater(view.takeFocus)

    Rectangle {
        anchors.fill: parent
        color: Qt.rgba(0, 0, 0, 0.35)
    }

    MouseArea {
        anchors.fill: parent
        onClicked: popup.model.requestClose()
    }

    Rectangle {
        id: card
        width: Math.min(view.implicitWidth + 32, popup.width - 32)
        height: view.implicitHeight + 32
        anchors.horizontalCenter: parent.horizontalCenter
        y: Math.max(16, Math.round(popup.height * 0.18))
        radius: popup.theme.radius === undefined ? 8 : popup.theme.radius
        color: popup.theme.background || "#1b1d20"
        border.width: 1
        border.color: popup.theme.border || "#3a3d42"

        MouseArea { anchors.fill: parent }

        PorticoView {
            id: view
            anchors.fill: parent
            anchors.margins: 16
            model: popup.model
            theme: popup.theme
            maxListHeight: Math.max(160, popup.height - card.y - 160)
        }
    }
}
