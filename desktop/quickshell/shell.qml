import QtQuick
import Quickshell
import Quickshell.Io

// Entry point for a plain Quickshell setup:
//
//   qs -p desktop/quickshell                       # start it (hidden)
//   qs -p desktop/quickshell ipc call portico toggle
//
// The popup opens and closes over IPC (toggle, open, close), so bind the
// toggle to a key. Nothing runs while it is closed.
ShellRoot {
    id: root

    property bool opened: false

    function open() {
        if (opened)
            return
        opened = true
        porticoModel.open()
    }

    function close() {
        opened = false
        porticoModel.close()
    }

    PorticoModel {
        id: porticoModel
        onRequestClose: root.close()
    }

    PorticoPopup {
        model: porticoModel
        opened: root.opened
    }

    IpcHandler {
        target: "portico"

        function toggle(): void {
            if (root.opened)
                root.close()
            else
                root.open()
        }

        function open(): void {
            root.open()
        }

        function close(): void {
            root.close()
        }
    }
}
