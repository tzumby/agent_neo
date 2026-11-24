import AgentAPI
import Observation
import UserNotifications

#if os(iOS)
    import UIKit
#else
    import AppKit
#endif

/// Registers this device for APNs and opens the conversation and card
/// mentioned in a notification. The app delegates (iOS/, macOS/) hand it the token.
@MainActor
@Observable
final class PushCenter: NSObject {
    static let shared = PushCenter()

    /// Hex device token, once the OS hands it over.
    private(set) var deviceToken: String?
    /// The chat row a notification points to.
    var pendingCardID: String?
    /// The conversation a notification opens (the home shows it, then its
    /// chat looks for `pendingCardID`). The chat's banner about another
    /// conversation opens it the same way.
    var pendingSession: String?
    @ObservationIgnored private var registrations: [URL: Task<String, Error>] = [:]
    /// Signing out: no new registrations, and Settings stays open to finish it.
    @ObservationIgnored private(set) var loggingOut = false
    #if os(macOS)
        /// Brings the chat window back (the Mac app runs on with it closed),
        /// so the user can see the card.
        @ObservationIgnored var showChat: (() -> Void)?
    #endif

    /// Call at launch so taps that open the app are delivered here.
    func setUp() {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.setNotificationCategories([])
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

    func registerDevice(on client: AgentClient, topic: String) async throws {
        try Task.checkCancellation()
        guard !loggingOut, let token = deviceToken else { return }
        if let pending = registrations[client.baseURL] {
            _ = try await pending.value
            return
        }
        let pending = Task {
            let id = try await client.registerDevice(
                token: token, name: Device.name, environment: Device.pushEnvironment, topic: topic)
            UserDefaults.standard.set(id, forKey: registrationKey(client))
            return id
        }
        registrations[client.baseURL] = pending
        defer { registrations[client.baseURL] = nil }
        _ = try await pending.value
    }

    /// Asks the desktop to stop notifying this device, before signing out of
    /// the tailnet that reaches it. Gives up after `limit`: signing out goes on
    /// either way. Returns whether the desktop confirmed (or had nothing to forget).
    func revokeBeforeLogout(on client: AgentClient, within limit: Duration = .seconds(5)) async -> Bool {
        loggingOut = true
        let key = registrationKey(client)
        let pending = registrations[client.baseURL]
        let revoke = Task { () -> Bool in
            if let pending {
                // Cancelling the revoke cancels a registration still under way.
                _ = try? await withTaskCancellationHandler {
                    try await pending.value
                } onCancel: {
                    pending.cancel()
                }
            }
            if Task.isCancelled { return false }  // out of time: it may have registered
            guard let id = UserDefaults.standard.string(forKey: key) else { return true }
            do {
                try await client.unregisterDevice(id: id)
            } catch let error as AgentError where error.statusCode == 404 {
                // The desktop already forgot this registration.
            } catch {
                return false  // keep the ID so a later sign-out can try again
            }
            UserDefaults.standard.removeObject(forKey: key)
            return true
        }
        let deadline = Task {
            try await Task.sleep(for: limit)
            revoke.cancel()
        }
        defer { deadline.cancel() }
        return await revoke.value
    }

    func finishLogout() {
        pendingCardID = nil
        pendingSession = nil
        loggingOut = false
    }

    private func registrationKey(_ client: AgentClient) -> String {
        "pushRegistrationID.\(client.baseURL.absoluteString)"
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
        let data = response.notification.request.content.userInfo
        let approval = data["approval"] as? String
        let action = data["action"] as? String
        // agentd 0.16.0 names the conversation; an older one has only main.
        let session = (data["session"] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? "main"
        nonisolated(unsafe) let done = completionHandler
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                if let approval, !approval.isEmpty {
                    self.pendingCardID = "approval-\(approval)"
                } else if let action, !action.isEmpty {
                    self.pendingCardID = "action-\(action)"
                }
                self.pendingSession = session
                #if os(macOS)
                    self.showChat?()
                #endif
            }
            done()
        }
    }
}
