import AppKit
import SwiftUI
import Tailnet

/// The Mac app: the chat in one window, the desktop (take over) in another.
/// Its own Tailscale node, as on the iPhone, so the Mac's Tailscale app isn't
/// needed. It keeps running with its windows closed, for notifications.
@main
struct CloudDesktopMacApp: App {
    @State private var tailnet = TailnetService(hostName: "cloud-desktop-mac")
    @State private var session = DesktopSession()
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate

    var body: some Scene {
        // One chat window: two would each stream the same chat.
        Window("Cloud Desktop", id: "chat") {
            Group {
                #if DEBUG
                    if let preview = ChatPreview.fromEnvironment() {
                        preview
                    } else if let preview = TakeOverPreview.fromEnvironment() {
                        preview
                    } else {
                        root
                    }
                #else
                    root
                #endif
            }
            .frame(minWidth: 420, minHeight: 520)
            .preferredColorScheme(.dark)
            .environment(PushCenter.shared)
            .environment(tailnet)
            .environment(session)
        }
        .defaultSize(width: 720, height: 860)
        .commands { ChatCommands() }

        Window("Your desktop", id: DesktopSession.desktopWindow) {
            DesktopWindow()
                .frame(minWidth: 640, minHeight: 440)
                .preferredColorScheme(.dark)
                .environment(tailnet)
                .environment(session)
        }
        .defaultSize(width: 1280, height: 860)
    }

    private var root: some View {
        RootView()
            .task { tailnet.start() }
            .modifier(ShowsChatForNotifications())
            // Connections don't survive sleep: start a fresh node (same identity)
            // on waking, as the iPhone does after a long trip to another app.
            .onReceive(NSWorkspace.shared.notificationCenter.publisher(for: NSWorkspace.didWakeNotification)) { _ in
                guard tailnet.phase == .running else { return }
                Task { await tailnet.restart() }
            }
    }
}

/// Lets a notification tap reopen the chat window after it was closed.
private struct ShowsChatForNotifications: ViewModifier {
    @Environment(\.openWindow) private var openWindow

    func body(content: Content) -> some View {
        content.onAppear {
            let openWindow = openWindow
            PushCenter.shared.showChat = { openWindow(id: "chat") }
        }
    }
}

/// Settings… (⌘,) opens the chat's Settings sheet.
struct OpenSettings {
    let open: () -> Void
}

extension FocusedValues {
    @Entry var openSettings: OpenSettings?
}

private struct ChatCommands: Commands {
    @FocusedValue(\.openSettings) private var settings

    var body: some Commands {
        CommandGroup(replacing: .appSettings) {
            Button("Settings…") { settings?.open() }
                .keyboardShortcut(",")
                .disabled(settings == nil)
        }
        // One chat window, no documents.
        CommandGroup(replacing: .newItem) {}
    }
}

/// Push: the device token, and notification taps (through PushCenter).
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        // Before launch finishes, so a tap that launched the app is delivered.
        MainActor.assumeIsolated { PushCenter.shared.setUp() }
    }

    func application(_ application: NSApplication, didRegisterForRemoteNotificationsWithDeviceToken token: Data) {
        MainActor.assumeIsolated { PushCenter.shared.didRegister(token: token) }
    }

    func application(_ application: NSApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        print("Push registration failed: \(error.localizedDescription)")
    }
}
