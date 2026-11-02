import AgentAPI
import AppKit
import ChatCore
import DesktopViewer
import SwiftUI
import Tailnet

/// The take-over window: your desktop, live, with the Mac's pointer and
/// keyboard. Opened from an Action needed card (I'm done finishes the card
/// and closes the window) or from the chat's toolbar.
struct DesktopWindow: View {
    @Environment(TailnetService.self) private var tailnet
    @Environment(DesktopSession.self) private var session

    var body: some View {
        if let desktop = tailnet.desktop, let client = tailnet.client(for: desktop),
            let endpoint = tailnet.endpoint(for: desktop)
        {
            MacTakeOverView(
                endpoint: endpoint, actionID: session.takeOverAction,
                store: session.store(for: desktop.host), client: client)
        } else {
            switch tailnet.phase {
            case .needsLogin:
                SplashView(status: "Sign in to Tailscale in the chat window")
            case .running:
                SplashView(status: "No desktop on your tailnet yet")
            default:
                SplashView(status: "Connecting to Tailscale…")
            }
        }
    }
}

struct MacTakeOverView: View {
    let endpoint: DesktopEndpoint
    let actionID: String?
    let store: ChatStore
    let client: AgentClient

    @Environment(\.dismissWindow) private var dismissWindow
    @State private var desktop = RemoteDesktop()

    /// The action as it is now (it may have been resolved meanwhile).
    private var action: Action? {
        for case .action(let action) in store.timeline.items where action.id == actionID && action.state == .pending {
            return action
        }
        return nil
    }

    var body: some View {
        DesktopCanvas(desktop: desktop, endpoint: endpoint)
            .overlay(alignment: .top) { status }
            .background(Color.black)
            .navigationTitle("Your desktop")
            .navigationSubtitle(action?.reason ?? "")
            .toolbar {
                ToolbarItem(placement: .navigation) {
                    Circle()
                        .fill(desktop.state == .live ? .green : .orange)
                        .frame(width: 8, height: 8)
                        .help(desktop.state == .live ? "Connected" : "Connecting")
                }
                ToolbarItemGroup(placement: .primaryAction) {
                    Button("Paste", systemImage: "doc.on.clipboard") {
                        if let text = NSPasteboard.general.string(forType: .string) { desktop.type(text) }
                    }
                    .help("Type the Mac's clipboard on the desktop (⌘V)")
                    if let action {
                        Button {
                            Task {
                                await store.resolve(action, done: true, client: client)
                                dismissWindow(id: DesktopSession.desktopWindow)
                            }
                        } label: {
                            Text("I'm done").padding(.horizontal, 6)
                        }
                        .buttonStyle(.borderedProminent)
                        .help("Tell the agent to carry on")
                    }
                }
            }
    }

    @ViewBuilder private var status: some View {
        switch desktop.state {
        case .live:
            EmptyView()
        case .connecting:
            if desktop.size == .zero {
                Label("Connecting to your desktop…", systemImage: "display")
                    .font(.subheadline.weight(.medium))
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .glass(.capsule)
                    .padding(.top, 12)
            }
        case .offline(let reason):
            VStack(spacing: 4) {
                Label("Reconnecting…", systemImage: "wifi.exclamationmark").font(.subheadline.weight(.semibold))
                Text(reason).font(.caption).foregroundStyle(.secondary).multilineTextAlignment(.center)
            }
            .padding(.horizontal, 18)
            .padding(.vertical, 10)
            .glass(.rect(cornerRadius: 20))
            .padding(12)
        }
    }
}

/// Hosts the Metal view and runs the VNC session while it's on screen
/// (again with a new endpoint, after Tailscale restarts).
private struct DesktopCanvas: NSViewRepresentable {
    let desktop: RemoteDesktop
    let endpoint: DesktopEndpoint

    final class Coordinator {
        var endpoint: DesktopEndpoint?
        var session: Task<Void, Never>?
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSView {
        guard let view = MacDesktopView(desktop: desktop) else {
            return NSHostingView(rootView: Text("This Mac can't draw the desktop (no Metal device).").padding())
        }
        start(view, context.coordinator)
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {
        guard let view = view as? MacDesktopView, context.coordinator.endpoint != endpoint else { return }
        start(view, context.coordinator)
    }

    static func dismantleNSView(_ view: NSView, coordinator: Coordinator) {
        coordinator.session?.cancel()
    }

    private func start(_ view: MacDesktopView, _ coordinator: Coordinator) {
        let previous = coordinator.session
        previous?.cancel()
        coordinator.endpoint = endpoint
        let (endpoint, desktop, renderer) = (endpoint, desktop, view.renderer)
        coordinator.session = Task {
            await previous?.value  // one session at a time on the view
            guard !Task.isCancelled else { return }
            await desktop.run(connect: { try await endpoint.connect() }, renderer: renderer)
        }
    }
}
