// Prints every key-down event this process receives, one per line:
// "<key code> <modifiers> <characters>", and pinch gestures as
// "magnify <change> <phase>". Used by the ignored tests
// `macos::tests::keys_reach_an_app` and `pinch_reaches_an_app`, which post
// events to this process only.
import AppKit

final class Receiver: NSApplication {
    override func sendEvent(_ event: NSEvent) {
        if event.type == .magnify {
            let phase: String
            switch event.phase {
            case .began: phase = "began"
            case .changed: phase = "changed"
            case .ended: phase = "ended"
            case .cancelled: phase = "cancelled"
            default: phase = "other"
            }
            print("magnify \(event.magnification) \(phase)")
            fflush(stdout)
        }
        if event.type == .keyDown {
            let f = event.modifierFlags
            var mods = ""
            if f.contains(.control) { mods += "ctrl+" }
            if f.contains(.option) { mods += "alt+" }
            if f.contains(.shift) { mods += "shift+" }
            if f.contains(.command) { mods += "cmd+" }
            print("\(event.keyCode) \(mods.isEmpty ? "-" : mods) \(event.characters ?? "")")
            fflush(stdout)
        }
        super.sendEvent(event)
    }
}

let app = Receiver.shared
app.setActivationPolicy(.accessory)
let seconds = Double(CommandLine.arguments.dropFirst().first ?? "5") ?? 5
DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { exit(0) }
DispatchQueue.main.async { print("ready"); fflush(stdout) }
app.run()
