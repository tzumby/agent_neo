import UIKit
import UserNotifications

/// Push notifications: asks permission, gets this iPhone's device token (the
/// desktop registers it with the push relay), and turns Approve/Deny taps on
/// approval notifications into answers the chat sends once it's connected.
@MainActor
@Observable
final class PushCenter: NSObject {
    static let shared = PushCenter()

    struct Answer: Equatable {
        let approvalID: String
        let approved: Bool
    }

    /// Hex device token, once iOS hands it over.
    private(set) var deviceToken: String?
    /// An Approve/Deny tapped on a notification, waiting to be sent.
    var pendingAnswer: Answer?

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
        let action = UNNotificationCategory(identifier: "ACTION", actions: [], intentIdentifiers: [])
        center.setNotificationCategories([approval, action])
    }

    /// Asks for permission (once) and registers for remote notifications.
    func register() async {
        let center = UNUserNotificationCenter.current()
        let granted = (try? await center.requestAuthorization(options: [.alert, .sound, .badge])) ?? false
        guard granted else { return }
        UIApplication.shared.registerForRemoteNotifications()
    }

    func didRegister(token: Data) {
        deviceToken = token.map { String(format: "%02x", $0) }.joined()
    }
}

// The completion-handler forms on purpose: with the async forms, Swift calls
// UIKit's completion handler from a background thread, and UIKit aborts the
// app (it asserts the main thread) as soon as a notification action is tapped.
extension PushCenter: UNUserNotificationCenterDelegate {
    /// The chat already shows everything while the app is open.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        nonisolated(unsafe) let done = completionHandler
        DispatchQueue.main.async { done([]) }
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
            }
            done()
        }
    }
}

/// Receives the device token from iOS.
final class AppDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        MainActor.assumeIsolated { PushCenter.shared.setUp() }
        return true
    }

    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken token: Data) {
        MainActor.assumeIsolated { PushCenter.shared.didRegister(token: token) }
    }

    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        print("Push registration failed: \(error.localizedDescription)")
    }
}
