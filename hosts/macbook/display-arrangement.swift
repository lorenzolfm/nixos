// Arranges the desk's displays left to right as Samsung LU28R55, ASUS VG32VQ1B, built-in, with
// the ASUS as the main display. macOS forgets or scrambles this when the MacBook lid opens, so
// the agent reapplies it on every display reconfiguration. It does nothing unless all three
// displays are connected, and nothing when they are already in place, which also stops its own
// reconfiguration from triggering it again.
//
// Run with `--once` to apply the arrangement and exit.

import AppKit
import CoreGraphics

let leftName = "LU28R55"
let mainName = "VG32VQ1B"

func displayID(named name: String) -> CGDirectDisplayID? {
    for screen in NSScreen.screens where screen.localizedName.contains(name) {
        return screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID
    }
    return nil
}

func builtinDisplayID() -> CGDirectDisplayID? {
    var ids = [CGDirectDisplayID](repeating: 0, count: 16)
    var count: UInt32 = 0
    guard CGGetOnlineDisplayList(UInt32(ids.count), &ids, &count) == .success else { return nil }
    return ids[0..<Int(count)].first { CGDisplayIsBuiltin($0) != 0 && CGDisplayIsActive($0) != 0 }
}

func arrange() {
    guard let left = displayID(named: leftName),
          let main = displayID(named: mainName),
          let builtin = builtinDisplayID()
    else { return }

    let mainSize = CGDisplayBounds(main).size
    let leftSize = CGDisplayBounds(left).size
    let builtinSize = CGDisplayBounds(builtin).size

    // Global coordinates have their origin at the main display's top-left corner, y pointing down.
    // The Samsung is top-aligned with the ASUS, as in Hyprland; the MacBook is bottom-aligned.
    let targets: [(CGDirectDisplayID, CGPoint)] = [
        (main, CGPoint(x: 0, y: 0)),
        (left, CGPoint(x: -leftSize.width, y: 0)),
        (builtin, CGPoint(x: mainSize.width, y: mainSize.height - builtinSize.height)),
    ]

    if targets.allSatisfy({ CGDisplayBounds($0.0).origin == $0.1 }) { return }

    var config: CGDisplayConfigRef?
    guard CGBeginDisplayConfiguration(&config) == .success else { return }
    for (id, origin) in targets {
        CGConfigureDisplayOrigin(config, id, Int32(origin.x), Int32(origin.y))
    }
    let result = CGCompleteDisplayConfiguration(config, .permanently)
    print("display-arrangement: applied, result \(result.rawValue)")
}

// NSScreen needs an application connection to the window server to see display changes.
_ = NSApplication.shared

if CommandLine.arguments.contains("--once") {
    arrange()
    exit(0)
}

// A single change fires several callbacks, so wait for them to settle before arranging.
var pending: DispatchWorkItem?
CGDisplayRegisterReconfigurationCallback({ _, flags, _ in
    if flags.contains(.beginConfigurationFlag) { return }
    DispatchQueue.main.async {
        pending?.cancel()
        let work = DispatchWorkItem { arrange() }
        pending = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 2, execute: work)
    }
}, nil)

arrange()
NSApplication.shared.run()
