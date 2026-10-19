import QtQuick
import qs.Commons

// Omarchy `panel` entry point: the launcher-style popup, summoned with
//
//   omarchy-shell shell toggle portico '{}'
//
// (bind it to a key). The shell calls open() and close(); the manifest keeps
// it loaded, so a start or stop still running when the popup closes finishes.
Item {
    id: root

    property var shell: null
    property var manifest: null
    property bool opened: false

    readonly property string pluginId: manifest && manifest.id ? String(manifest.id) : "portico"

    function open(payloadJson) {
        if (opened)
            return
        opened = true
        porticoModel.open()
    }

    function close() {
        opened = false
        porticoModel.close()
    }

    function requestClose() {
        if (shell && typeof shell.hide === "function")
            shell.hide(pluginId)
        else
            close()
    }

    PorticoModel {
        id: porticoModel
        onRequestClose: root.requestClose()
    }

    PorticoPopup {
        model: porticoModel
        opened: root.opened
        theme: ({
            foreground: Color.popups.text,
            dim: Color.muted,
            accent: Color.accent,
            urgent: Color.urgent,
            background: Color.popups.background,
            border: Color.popups.border,
            fontFamily: Style.fontFamily,
            fontSize: Style.fontBaseSize + 1
        })
    }
}
