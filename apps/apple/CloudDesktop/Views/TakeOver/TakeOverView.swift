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
                .overlay(alignment: .top) { status }
                .background(Color.black)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .topBarLeading) {
                        Button("Close", systemImage: "xmark") { dismiss() }
                    }
                    ToolbarItem(placement: .principal) {
                        VStack(spacing: 1) {
                            HStack(spacing: 6) {
                                Circle().fill(desktop.state == .live ? .green : .orange).frame(width: 7, height: 7)
                                Text("Your desktop").font(.headline)
                            }
                            if let action, action.state == .pending {
                                Text(action.reason).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                            }
                        }
                        .frame(maxWidth: 260)
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
                Label("Connecting to your desktop…", systemImage: "display")
                    .font(.subheadline.weight(.medium))
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .glassEffect(.regular, in: .capsule)
                    .padding(.top, 12)
            }
        case .offline(let reason):
            VStack(spacing: 4) {
                Label("Reconnecting…", systemImage: "wifi.exclamationmark").font(.subheadline.weight(.semibold))
                Text(reason).font(.caption).foregroundStyle(.secondary).multilineTextAlignment(.center)
            }
            .padding(.horizontal, 18)
            .padding(.vertical, 10)
            .glassEffect(.regular, in: .rect(cornerRadius: 20))
            .padding(12)
        }
    }

    /// Floating glass controls, as in Grok: paste, I'm done, keyboard.
    private var bottomBar: some View {
        GlassEffectContainer(spacing: 12) {
            HStack(spacing: 12) {
                // Types the phone's clipboard on the desktop (PasteButton needs no
                // clipboard permission prompt).
                PasteButton(payloadType: String.self) { strings in
                    let text = strings.joined()
                    Task { @MainActor in desktop.type(text) }
                }
                .labelStyle(.iconOnly)
                .buttonBorderShape(.circle)
                .controlSize(.large)
                .tint(.white.opacity(0.18))

                Spacer(minLength: 0)
                if let action, action.state == .pending {
                    Button {
                        Task {
                            await store.resolve(action, done: true, client: client)
                            dismiss()
                        }
                    } label: {
                        Text("I'm done").font(.headline).foregroundStyle(.black).padding(.horizontal, 18)
                    }
                    .primaryGlass()
                    .controlSize(.large)
                }
                Spacer(minLength: 0)

                Button {
                    keyboardUp.toggle()
                } label: {
                    Image(systemName: keyboardUp ? "keyboard.chevron.compact.down" : "keyboard")
                        .font(.title3)
                        .frame(width: 30, height: 30)
                }
                .accessibilityLabel(keyboardUp ? "Hide keyboard" : "Show keyboard")
                .buttonStyle(.glass)
                .buttonBorderShape(.circle)
                .controlSize(.large)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
        }
        .sensoryFeedback(.impact(weight: .light), trigger: keyboardUp)
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
