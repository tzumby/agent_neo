import UIKit

/// Asks iOS for its background grace time (about 30 seconds) when the app
/// leaves the screen, so a quick trip to another app, e.g. for a 2FA code,
/// doesn't suspend it and drop the tailnet, the chat stream or the desktop view.
@MainActor
final class BackgroundGrace {
    private var task = UIBackgroundTaskIdentifier.invalid
    private var expired = false

    func begin() {
        guard task == .invalid else { return }
        expired = false
        task = UIApplication.shared.beginBackgroundTask(withName: "Stay connected to the desktop") { [weak self] in
            // Time's up: iOS suspends the app next.
            MainActor.assumeIsolated {
                self?.expired = true
                self?.finishTask()
            }
        }
    }

    /// Back in the foreground. Returns whether the grace time ran out, so the
    /// app was (or may have been) suspended.
    func end() -> Bool {
        finishTask()
        return expired
    }

    private func finishTask() {
        guard task != .invalid else { return }
        UIApplication.shared.endBackgroundTask(task)
        task = .invalid
    }
}
