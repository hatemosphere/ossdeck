// Prints the CGWindowID of ossdeck's main window for the given PID (for screencapture -l).
import CoreGraphics
let pid = Int(CommandLine.arguments[1])!
let list = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as! [[String: Any]]
for w in list where (w[kCGWindowOwnerPID as String] as? Int) == pid
    && ((w[kCGWindowName as String] as? String) ?? "").hasPrefix("ossdeck") {
    print(w[kCGWindowNumber as String]!)
    break
}
