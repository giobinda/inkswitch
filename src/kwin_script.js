// Loaded into KWin by `inkswitch daemon`; reports the focused window over D-Bus.
function report(w) {
    if (!w || !w.normalWindow) {
        return;
    }
    callDBus("io.github.giobinda.Inkswitch", "/io/github/giobinda/Inkswitch", "io.github.giobinda.Inkswitch", "Focus",
             String(w.resourceClass || ""), String(w.resourceName || ""), String(w.caption || ""));
}
workspace.windowActivated.connect(report);
report(workspace.activeWindow);
