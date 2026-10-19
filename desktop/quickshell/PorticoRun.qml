import QtQuick
import Quickshell.Io

// One run of one command. `finished` comes once, after the process has exited
// and both of its streams have been read to their end; `notStarted` comes
// instead when the program could not be started (Quickshell then turns
// `running` false without `started` and never emits `exited`). The model
// destroys the run afterwards.
Process {
    id: run

    signal finished(string stdout, string stderr, int exitCode)
    signal notStarted()

    property bool began: false
    property bool ended: false
    property bool outDone: false
    property bool errDone: false
    property bool reported: false
    property int code: -1

    function settle() {
        if (reported || !ended || !outDone || !errDone)
            return
        reported = true
        finished(out.text, err.text, code)
    }

    stdout: StdioCollector {
        id: out
        onStreamFinished: { run.outDone = true; run.settle() }
    }
    stderr: StdioCollector {
        id: err
        onStreamFinished: { run.errDone = true; run.settle() }
    }

    onStarted: began = true
    onExited: function (exitCode, exitStatus) {
        code = exitCode
        ended = true
        settle()
    }
    onRunningChanged: {
        if (!running && !began && !reported) {
            reported = true
            notStarted()
        }
    }
}
