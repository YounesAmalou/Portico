import QtQuick
import qs.Commons
import qs.Ui

// Omarchy bar widget: an icon whose click opens the panel as a dropdown under
// it, with Omarchy's own KeyboardPanel, as the first-party popups do. The
// widget owns the model, so a start or stop outlives the dropdown.
BarWidget {
    id: root
    moduleName: "portico"

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

    function toggle() {
        if (opened)
            close()
        else
            open()
    }

    implicitWidth: button.implicitWidth
    implicitHeight: button.implicitHeight

    PorticoModel {
        id: porticoModel
        onRequestClose: root.close()
    }

    BarIconButton {
        id: button
        anchors.fill: parent
        bar: root.bar
        text: "󰣀"
        tooltipText: "Portico"
        useActiveColor: false
        onPressed: function (b) { root.toggle() }
    }

    KeyboardPanel {
        id: kpanel
        anchorItem: button
        owner: root
        bar: root.bar
        open: root.opened
        focusTarget: view
        contentWidth: kpanel.fittedContentWidth(Style.space(440))
        contentHeight: kpanel.fittedContentHeight(view.implicitHeight)

        PorticoView {
            id: view
            anchors.fill: parent
            model: porticoModel
            maxListHeight: Style.space(460)
            theme: ({
                foreground: root.bar ? root.bar.foreground : Color.popups.text,
                dim: Color.muted,
                accent: Color.accent,
                urgent: Color.urgent,
                border: Color.popups.border,
                fontFamily: root.bar ? root.bar.fontFamily : Style.fontFamily,
                fontSize: Style.fontBaseSize + 1
            })
        }
    }
}
