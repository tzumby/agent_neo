import AgentAPI
import ChatCore
import DesktopViewer
import SwiftUI

struct ChatView: View {
    let store: ChatStore
    let client: AgentClient
    let desktopName: String
    let endpoint: DesktopEndpoint?
    let connectionKey: String

    @State private var draft = ""
    @State private var showingSettings = false
    @State private var takeOver: TakeOverRequest?
    @State private var position = ScrollPosition(edge: .bottom)
    @State private var sent = 0
    @FocusState private var composing: Bool
    @Environment(PushCenter.self) private var push

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 20) {
                ForEach(store.timeline.items) { item in
                    ChatRow(item: item).equatable()
                }
            }
            .padding(.horizontal, 18)
            .padding(.vertical, 12)
        }
        .scrollPosition($position)
        // Start at the newest message and stay there as the agent adds more.
        .defaultScrollAnchor(.bottom, for: .initialOffset)
        .defaultScrollAnchor(.bottom, for: .sizeChanges)
        .scrollDismissesKeyboard(.interactively)
        .overlay {
            if store.timeline.items.isEmpty, store.connection == .live {
                EmptyChat(desktopName: desktopName)
            }
        }
        .safeAreaBar(edge: .bottom) {
            VStack(spacing: 10) {
                if store.timeline.items.isEmpty, store.connection == .live {
                    Suggestions { send($0) }
                }
                Composer(
                    text: $draft, busy: store.busy, canSend: store.connection == .live, focus: $composing,
                    onSend: { send(draft) }, onStop: { Task { await store.stop(client: client) } })
            }
            .padding(.horizontal, 12)
            .padding(.bottom, 6)
        }
        .background(Color.black)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar { toolbar }
        .sheet(isPresented: $showingSettings) {
            NavigationStack { SettingsView(store: store, client: client) }
        }
        .fullScreenCover(item: $takeOver) { request in
            if let endpoint {
                TakeOverView(endpoint: endpoint, actionID: request.actionID, store: store, client: client)
            } else {
                // Tailscale is restarting (e.g. the app was in the background).
                VStack(spacing: 16) {
                    ProgressView("Reconnecting to Tailscale…")
                    Button("Close") { takeOver = nil }.buttonStyle(.glass)
                }
            }
        }
        .alert(
            "Cloud Desktop",
            isPresented: Binding(get: { store.notice != nil }, set: { if !$0 { store.notice = nil } })
        ) {
            Button("OK") {}
        } message: {
            Text(store.notice ?? "")
        }
        .sensoryFeedback(.impact(weight: .light), trigger: sent)
        .environment(\.chatStore, store)
        .environment(\.agentClient, client)
        .environment(\.takeOver, endpoint == nil ? nil : TakeOverAction { takeOver = TakeOverRequest(actionID: $0.id) })
        // Until the first sync, the splash covers everything (bars included).
        .overlay {
            if loading {
                SplashView(status: "Connecting to \(desktopName)…", detail: offlineDetail)
                    .transition(.opacity)
            }
        }
        .toolbarVisibility(loading ? .hidden : .automatic, for: .navigationBar)
        .animation(.easeOut(duration: 0.25), value: loading)
        .task(id: connectionKey) { await store.run(client: client) }
        // Ask for notifications once the chat is up, not over the splash.
        .task(id: store.connection == .live) {
            if store.connection == .live { await push.register() }
        }
        .task(id: "\(push.deviceToken ?? "")|\(connectionKey)") { await registerForPush() }
        .task(id: push.pendingAnswer) { await sendPendingAnswer() }
    }

    /// Hands this iPhone's push token to the desktop, which registers it with
    /// the push relay. Retries while the connection comes up.
    private func registerForPush() async {
        guard let token = push.deviceToken else { return }
        for _ in 0..<10 {
            if store.connection == .live {
                do {
                    try await client.registerDevice(token: token, name: UIDevice.current.name)
                    return
                } catch let error as AgentError where error.statusCode == 503 {
                    return  // push isn't set up on this desktop
                } catch {}
            }
            try? await Task.sleep(for: .seconds(3))
            if Task.isCancelled { return }
        }
    }

    /// Approve/Deny tapped on a notification: send it once connected.
    private func sendPendingAnswer() async {
        guard let answer = push.pendingAnswer else { return }
        // After a cold launch from the notification, Tailscale needs a few seconds.
        for _ in 0..<45 where store.connection != .live {
            try? await Task.sleep(for: .seconds(1))
            if Task.isCancelled { return }
        }
        do {
            try await client.answer(approval: answer.approvalID, approved: answer.approved)
        } catch let error as AgentError where error.statusCode == 404 {
            store.notice = "That request has expired."
        } catch {
            store.notice = "Couldn't send your answer: \(error.localizedDescription)"
        }
        push.pendingAnswer = nil
    }

    /// The first sync with the desktop hasn't finished yet.
    private var loading: Bool { store.status == nil && store.timeline.items.isEmpty }

    private var offlineDetail: String? {
        if case .offline(let reason) = store.connection { return "Still trying: \(reason)" }
        return nil
    }

    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        ToolbarItem(placement: .topBarLeading) {
            Button("Settings", systemImage: "slider.horizontal.3") { showingSettings = true }
        }
        ToolbarItem(placement: .principal) {
            ModelMenu(store: store, client: client, desktopName: desktopName)
        }
        ToolbarItem(placement: .topBarTrailing) {
            Button("Your desktop", systemImage: "display") { takeOver = TakeOverRequest(actionID: nil) }
                .disabled(endpoint == nil)
        }
    }

    private func send(_ text: String) {
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        draft = ""
        sent += 1
        withAnimation(.snappy) { position.scrollTo(edge: .bottom) }
        Task { await store.send(text, client: client) }
    }
}

// MARK: - Composer

/// The floating input: a glass field that grows to six lines, with a white
/// send button that turns into Stop while the agent works.
private struct Composer: View {
    @Binding var text: String
    let busy: Bool
    let canSend: Bool
    var focus: FocusState<Bool>.Binding
    let onSend: () -> Void
    let onStop: () -> Void

    private var empty: Bool { text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }

    var body: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextField("Ask your desktop", text: $text, axis: .vertical)
                .lineLimit(1...6)
                .focused(focus)
                .padding(.leading, 18)
                .padding(.vertical, 14)
            Group {
                if busy {
                    Button(action: onStop) {
                        Image(systemName: "stop.fill").font(.system(size: 14, weight: .bold))
                    }
                    .accessibilityLabel("Stop")
                } else {
                    Button(action: onSend) {
                        Image(systemName: "arrow.up").font(.system(size: 16, weight: .bold))
                    }
                    .accessibilityLabel("Send")
                    .disabled(empty || !canSend)
                }
            }
            .buttonStyle(SendButtonStyle())
            .padding(6)
            .animation(.snappy(duration: 0.2), value: busy)
        }
        .glassEffect(.regular.interactive(), in: .rect(cornerRadius: 26))
    }
}

/// A white circle with a black glyph, dimmed when disabled; shrinks on press.
private struct SendButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(.black)
            .frame(width: 38, height: 38)
            .background(.white.opacity(isEnabled ? 1 : 0.25), in: .circle)
            .scaleEffect(configuration.isPressed ? 0.9 : 1)
            .animation(.snappy(duration: 0.15), value: configuration.isPressed)
    }
}

// MARK: - Title: model and connection

/// The centre of the bar, as in Grok: the model, with a menu to switch it,
/// and a dot for the connection to the desktop.
private struct ModelMenu: View {
    let store: ChatStore
    let client: AgentClient
    let desktopName: String

    var body: some View {
        Menu {
            if let status = store.status {
                Picker("Model", selection: modelBinding(status)) {
                    ForEach(status.knownModels, id: \.self) { Text(shortName($0)).tag($0) }
                }
                Toggle("Browser actions", isOn: actionsBinding(status))
            }
            Section(desktopName) {
                Text(connectionText)
            }
        } label: {
            HStack(spacing: 6) {
                Circle().fill(dotColor).frame(width: 7, height: 7)
                Text(store.status.map { shortName($0.settings.model) } ?? desktopName)
                    .font(.headline)
                    .lineLimit(1)
                Image(systemName: "chevron.down").font(.caption2.weight(.bold)).foregroundStyle(.secondary)
            }
            .padding(.horizontal, 4)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Model and connection")
    }

    private var dotColor: Color {
        switch store.connection {
        case .live: .green
        case .connecting: .gray
        case .offline: .orange
        }
    }

    private var connectionText: String {
        switch store.connection {
        case .live: "Connected"
        case .connecting: "Connecting…"
        case .offline(let reason): "Offline: \(reason)"
        }
    }

    private func shortName(_ model: String) -> String {
        model.split(separator: "/").last.map(String.init) ?? model
    }

    private func modelBinding(_ status: AgentStatus) -> Binding<String> {
        Binding(
            get: { status.settings.model },
            set: { model in
                var settings = status.settings
                settings.model = model
                Task { await store.update(settings: settings, client: client) }
            })
    }

    private func actionsBinding(_ status: AgentStatus) -> Binding<Bool> {
        Binding(
            get: { status.settings.allowActions },
            set: { allow in
                var settings = status.settings
                settings.allowActions = allow
                Task { await store.update(settings: settings, client: client) }
            })
    }
}

// MARK: - Empty chat

private struct EmptyChat: View {
    let desktopName: String

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "sparkles")
                .font(.system(size: 34, weight: .light))
                .frame(width: 76, height: 76)
                .glassEffect(.regular, in: .circle)
            Text("What should your desktop do?")
                .font(.title2.weight(.semibold))
                .multilineTextAlignment(.center)
            Text(desktopName)
                .font(.subheadline)
                .foregroundStyle(.secondary)
        }
        .padding(.bottom, 80)
        .allowsHitTesting(false)
    }
}

/// Starter requests above the composer while the chat is empty.
private struct Suggestions: View {
    let pick: (String) -> Void

    private let items = [
        ("envelope", "Summarize my inbox"),
        ("number", "Any Slack mentions?"),
        ("bell", "Open my GitHub notifications"),
    ]

    var body: some View {
        ScrollView(.horizontal) {
            GlassEffectContainer(spacing: 8) {
                HStack(spacing: 8) {
                    ForEach(items, id: \.1) { icon, text in
                        Button { pick(text) } label: {
                            Label(text, systemImage: icon).font(.subheadline)
                        }
                        .buttonStyle(.glass)
                    }
                }
                .padding(.horizontal, 4)
            }
        }
        .scrollIndicators(.hidden)
    }
}
