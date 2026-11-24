import AgentAPI
import ChatCore
import DesktopViewer
import SwiftUI
import UIKit

/// What the take-over screen is opened for: an Action needed card, or just
/// to look at the desktop (no action).
struct TakeOverRequest: Identifiable {
    let id = UUID()
    let actionID: String?
}

/// The desktop full screen with the native keyboard, as in Grok's "Take over":
/// a bottom bar with Copy, Paste, I'm done and the keyboard toggle.
struct TakeOverView: View {
    let endpoint: DesktopEndpoint
    let actionID: String?
    let store: ChatStore
    let client: AgentClient
    var opensKeyboard = false

    @Environment(\.dismiss) private var dismiss
    @State private var desktop = RemoteDesktop()
    @State private var keyboardUp = false
    /// What Copy just did, shown for a moment.
    @State private var copyNote: String?
    /// I'm done was tapped and the card is being resolved.
    @State private var finishing = false

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
                .overlay(alignment: .bottom) { copied }
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

    @ViewBuilder private var copied: some View {
        if let copyNote {
            Text(copyNote)
                .font(.subheadline.weight(.medium))
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
                .glassEffect(.regular, in: .capsule)
                .padding(.bottom, 12)
                .transition(.opacity)
        }
    }

    /// Brings what's selected on the desktop to the phone, and only when asked.
    private func copySelection() {
        desktop.copySelection { text in
            if let text { Clipboard.copy(text) }
            let note = text == nil ? "Nothing selected. Long-press and drag over the text first." : "Copied to your phone"
            withAnimation { copyNote = note }
            Task {
                try? await Task.sleep(for: .seconds(2.5))
                if copyNote == note { withAnimation { copyNote = nil } }
            }
        }
    }

    /// Marks the Action needed card done, then closes.
    private func finish(_ action: Action) {
        finishing = true
        Task {
            await store.resolve(action, done: true, client: client)
            dismiss()
        }
    }

    /// The bottom bar: copy, paste, I'm done, keyboard. The round buttons are
    /// all one size and colour, so they're drawn to match the paste control
    /// (Apple draws that one), and I'm done is a capsule of the same height.
    private var bottomBar: some View {
        HStack(spacing: 12) {
            Button(action: copySelection) {
                Image(systemName: "doc.on.doc")
            }
            .buttonStyle(RoundButtonStyle())
            .accessibilityLabel("Copy what's selected on the desktop")

            // Types the phone's clipboard on the desktop, without the clipboard
            // permission prompt.
            PasteControl { text in desktop.type(text) }
                .accessibilityLabel("Paste your phone's clipboard on the desktop")

            Spacer(minLength: 12)
            if let action, action.state == .pending {
                Button {
                    finish(action)
                } label: {
                    if finishing {
                        ProgressView().tint(.black)
                    } else {
                        Text("I'm done")
                    }
                }
                .buttonStyle(DoneButtonStyle())
                .disabled(finishing)
            }
            Spacer(minLength: 12)

            Button {
                keyboardUp.toggle()
            } label: {
                Image(systemName: keyboardUp ? "keyboard.chevron.compact.down" : "keyboard")
            }
            .buttonStyle(RoundButtonStyle())
            .accessibilityLabel(keyboardUp ? "Hide keyboard" : "Show keyboard")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
        // I'm done stays on one line; the round buttons have a fixed size.
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
        .sensoryFeedback(.impact(weight: .light), trigger: keyboardUp)
        .sensoryFeedback(.success, trigger: copyNote) { _, note in note != nil }
    }
}

/// The bottom bar's measurements, shared by the buttons and the paste control.
private enum Bar {
    /// The round buttons' diameter, and I'm done's height.
    static let size: CGFloat = 52
    /// The round buttons' fill: a dark grey that reads on the black around the desktop.
    static let fill: CGFloat = 0.17
    /// The round buttons' symbols. The paste control's glyph follows the text
    /// size instead, and `PasteControl.Host.glyphSize` matches it to this one
    /// (compared side by side in the iOS 26 simulator).
    static let symbol: CGFloat = 17
}

/// A round icon button drawn like the paste control beside it.
private struct RoundButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: Bar.symbol, weight: .medium))
            .foregroundStyle(.white.opacity(isEnabled ? 1 : 0.4))
            .frame(width: Bar.size, height: Bar.size)
            .background(Circle().fill(Color(white: Bar.fill)))
            .contentShape(.circle)
            .opacity(configuration.isPressed ? 0.6 : 1)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

/// I'm done: a white capsule, as tall as the round buttons, on one line.
private struct DoneButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.headline)
            .lineLimit(1)
            .fixedSize()
            .foregroundStyle(.black)
            .padding(.horizontal, 26)
            .frame(minWidth: 120, minHeight: Bar.size, maxHeight: Bar.size)
            .background(Capsule().fill(.white))
            .contentShape(.capsule)
            .opacity(configuration.isPressed ? 0.75 : 1)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

/// UIKit's paste control at the bar's size. SwiftUI's PasteButton draws itself
/// smaller than the buttons beside it, and can't be resized or restyled. Like
/// PasteButton, it pastes without the clipboard permission prompt.
private struct PasteControl: UIViewRepresentable {
    var onPaste: @MainActor (String) -> Void

    func makeUIView(context: Context) -> Host {
        let host = Host()
        host.onPaste = onPaste
        return host
    }

    func updateUIView(_ host: Host, context: Context) {
        host.onPaste = onPaste
    }

    func sizeThatFits(_ proposal: ProposedViewSize, uiView: Host, context: Context) -> CGSize? {
        CGSize(width: Bar.size, height: Bar.size)
    }

    /// The paste target: the control hands it what's on the clipboard.
    final class Host: UIView {
        /// The control's glyph follows the text size, not a font, so this size
        /// is the one whose glyph matches `Bar.symbol`.
        static let glyphSize = UIContentSizeCategory.extraExtraLarge

        var onPaste: @MainActor (String) -> Void = { _ in }

        init() {
            let configuration = UIPasteControl.Configuration()
            configuration.displayMode = .iconOnly
            configuration.cornerStyle = .capsule
            configuration.baseForegroundColor = .white
            configuration.baseBackgroundColor = UIColor(white: Bar.fill, alpha: 1)
            let control = UIPasteControl(configuration: configuration)
            super.init(frame: CGRect(x: 0, y: 0, width: Bar.size, height: Bar.size))
            pasteConfiguration = UIPasteConfiguration(forAccepting: NSString.self)
            traitOverrides.preferredContentSizeCategory = Self.glyphSize
            control.target = self
            control.translatesAutoresizingMaskIntoConstraints = false
            addSubview(control)
            NSLayoutConstraint.activate([
                control.leadingAnchor.constraint(equalTo: leadingAnchor),
                control.trailingAnchor.constraint(equalTo: trailingAnchor),
                control.topAnchor.constraint(equalTo: topAnchor),
                control.bottomAnchor.constraint(equalTo: bottomAnchor),
            ])
        }

        required init?(coder: NSCoder) { nil }

        override func paste(itemProviders: [NSItemProvider]) {
            Task { @MainActor in
                var texts: [String] = []
                for provider in itemProviders where provider.canLoadObject(ofClass: String.self) {
                    if let text = await provider.loadText() { texts.append(text) }
                }
                if !texts.isEmpty { onPaste(texts.joined()) }
            }
        }
    }
}

private extension NSItemProvider {
    @MainActor func loadText() async -> String? {
        await withCheckedContinuation { continuation in
            _ = loadObject(ofClass: String.self) { text, _ in continuation.resume(returning: text) }
        }
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
