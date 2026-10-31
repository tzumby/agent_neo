import AgentAPI
import ChatCore
import DesktopViewer
import SwiftUI

/// What the take-over screen is opened for: an Action needed card, or just
/// to look at the desktop (no action).
struct TakeOverRequest: Identifiable {
    let id = UUID()
    let actionID: String?
}

/// The desktop full screen with the native keyboard, as in Grok's "Take over":
/// a bottom bar with Paste, I'm done and the keyboard toggle.
struct TakeOverView: View {
    let endpoint: DesktopEndpoint
    let actionID: String?
    let store: ChatStore
    let client: AgentClient
    var opensKeyboard = false

    @Environment(\.dismiss) private var dismiss
    @State private var desktop = RemoteDesktop()
    @State private var keyboardUp = false

    /// The action as it is now (it may have been resolved meanwhile).
    private var action: Action? {
        for case .action(let action) in store.timeline.items where action.id == actionID {
            return action
        }
        return nil
    }

    var body: some View {
        NavigationStack {
            DesktopCanvas(desktop: desktop, keyboardUp: $keyboardUp)
                .overlay { status }
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .topBarLeading) {
                        Button("Close", systemImage: "xmark") { dismiss() }
                    }
                    ToolbarItem(placement: .principal) {
                        VStack(spacing: 0) {
                            Text("Your desktop").font(.headline)
                            if let action, action.state == .pending {
                                Text(action.reason).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                            }
                        }
                    }
                }
                .safeAreaInset(edge: .bottom, spacing: 0) { bottomBar }
        }
        .onAppear { if opensKeyboard { keyboardUp = true } }
        .task(id: endpoint) {
            let endpoint = endpoint
            await desktop.run { try await endpoint.connect() }
        }
    }

    @ViewBuilder private var status: some View {
        switch desktop.state {
        case .live:
            EmptyView()
        case .connecting:
            if desktop.size == .zero {
                ProgressView("Connecting to your desktop…").tint(.white).foregroundStyle(.white)
            }
        case .offline(let reason):
            VStack(spacing: 8) {
                Label("Reconnecting…", systemImage: "wifi.exclamationmark").font(.headline)
                Text(reason).font(.caption).multilineTextAlignment(.center)
            }
            .padding()
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
            .padding()
        }
    }

    private var bottomBar: some View {
        HStack(spacing: 12) {
            // Types the phone's clipboard on the desktop (PasteButton needs no
            // clipboard permission prompt).
            PasteButton(payloadType: String.self) { strings in
                let text = strings.joined()
                Task { @MainActor in desktop.type(text) }
            }
            .labelStyle(.iconOnly)
            .buttonBorderShape(.capsule)

            Spacer()
            if let action, action.state == .pending {
                Button("I'm done") {
                    Task {
                        await store.resolve(action, done: true, client: client)
                        dismiss()
                    }
                }
                .buttonStyle(.borderedProminent)
                .buttonBorderShape(.capsule)
            }
            Spacer()

            Button(
                keyboardUp ? "Hide keyboard" : "Show keyboard",
                systemImage: keyboardUp ? "keyboard.chevron.compact.down" : "keyboard"
            ) {
                keyboardUp.toggle()
            }
            .labelStyle(.iconOnly)
            .font(.title3)
            .buttonStyle(.bordered)
            .buttonBorderShape(.circle)
        }
        .padding(.horizontal)
        .padding(.vertical, 8)
        .background(.bar)
    }
}

/// Hosts the UIKit canvas and keeps the keyboard in step with `keyboardUp`.
private struct DesktopCanvas: UIViewRepresentable {
    let desktop: RemoteDesktop
    @Binding var keyboardUp: Bool

    func makeUIView(context: Context) -> DesktopCanvasView {
        let view = DesktopCanvasView(desktop: desktop)
        let keyboardUp = $keyboardUp
        view.keyInput.onFocusChange = { focused in
            DispatchQueue.main.async {
                if keyboardUp.wrappedValue != focused { keyboardUp.wrappedValue = focused }
            }
        }
        return view
    }

    func updateUIView(_ view: DesktopCanvasView, context: Context) {
        let input = view.keyInput
        if keyboardUp, !input.isFirstResponder {
            DispatchQueue.main.async { input.becomeFirstResponder() }
        } else if !keyboardUp, input.isFirstResponder {
            input.resignFirstResponder()
        }
    }
}
