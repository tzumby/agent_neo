import Observation
import UserNotifications

#if os(iOS)
    import UIKit
#else
    import AppKit
#endif

/// Push notifications: asks permission, gets this device's token (agentd
/// registers it and sends to it over APNs), and turns Approve/Deny taps on
/// approval notifications into answers the chat sends once it's connected.
/// The app delegates (iOS/, macOS/) hand it the token.
@MainActor
@Observable
final class PushCenter: NSObject {
    static let shared = PushCenter()

    struct Answer: Equatable {
        let approvalID: String
        let approved: Bool
    }

    /// Hex device token, once the OS hands it over.
    private(set) var deviceToken: String?
    /// An Approve/Deny tapped on a notification, waiting to be sent.
    var pendingAnswer: Answer?
    #if os(macOS)
        /// Brings the chat window back (the Mac app runs on with it closed),
        /// so a notification's answer gets sent and its card seen.
        @ObservationIgnored var showChat: (() -> Void)?
    #endif

    nonisolated private static let approve = "APPROVE"
    nonisolated private static let deny = "DENY"

    /// Call at launch so taps that open the app are delivered here.
    func setUp() {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        // Approving opens the app (unlocked): the answer goes over the tailnet,
        // which the app connects to in the foreground.
        let approval = UNNotificationCategory(
            identifier: "APPROVAL",
            actions: [
                UNNotificationAction(identifier: Self.approve, title: "Approve", options: [.authenticationRequired, .foreground]),
                UNNotificationAction(identifier: Self.deny, title: "Deny", options: [.destructive, .authenticationRequired, .foreground]),
            ],
            intentIdentifiers: [])
        center.setNotificationCategories([approval])
    }

    /// Asks for permission (once) and registers for remote notifications.
    func register() async {
        #if DEBUG
            // The chat preview talks to a stand-in server, with no notifications.
            if ProcessInfo.processInfo.environment["CHAT_PREVIEW_URL"] != nil { return }
        #endif
        let center = UNUserNotificationCenter.current()
        let granted = (try? await center.requestAuthorization(options: [.alert, .sound, .badge])) ?? false
        guard granted else { return }
        #if os(iOS)
            UIApplication.shared.registerForRemoteNotifications()
        #else
            NSApplication.shared.registerForRemoteNotifications()
        #endif
    }

    func didRegister(token: Data) {
        deviceToken = token.map { String(format: "%02x", $0) }.joined()
    }
}

// The completion-handler forms on purpose: with the async forms, Swift calls
// UIKit's completion handler from a background thread, and UIKit aborts the
// app (it asserts the main thread) as soon as a notification action is tapped.
extension PushCenter: UNUserNotificationCenterDelegate {
    /// The chat already shows everything while the app is open. The Mac app
    /// keeps running behind other apps, though: there, show it unless it's in front.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        nonisolated(unsafe) let done = completionHandler
        DispatchQueue.main.async {
            #if os(macOS)
                done(NSApplication.shared.isActive ? [] : [.banner, .list, .sound])
            #else
                done([])
            #endif
        }
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let action = response.actionIdentifier
        let id = response.notification.request.content.userInfo["approval"] as? String
        nonisolated(unsafe) let done = completionHandler
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                if let id, action == Self.approve || action == Self.deny {
                    self.pendingAnswer = Answer(approvalID: id, approved: action == Self.approve)
                }
                #if os(macOS)
                    self.showChat?()
                #endif
            }
            done()
        }
    }
}
