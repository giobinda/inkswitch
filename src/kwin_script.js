// Loaded into KWin by `xppen-profiles daemon`; reports the focused window over D-Bus.
function report(w) {
    if (!w || !w.normalWindow) {
        return;
    }
    callDBus("dev.xppen.Profiles", "/dev/xppen/Profiles", "dev.xppen.Profiles", "Focus",
             String(w.resourceClass || ""), String(w.resourceName || ""), String(w.caption || ""));
}
workspace.windowActivated.connect(report);
report(workspace.activeWindow);
