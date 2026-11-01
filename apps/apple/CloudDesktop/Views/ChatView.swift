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
    @State private var showingMemory = false
    @State private var takeOver: TakeOverRequest?
    @State private var position = ScrollPosition(edge: .bottom)
    @State private var sent = 0
    /// A key in the message about to be sent: offer to save it instead.
    @State private var foundKey: KeyDetector.Found?
    @State private var savingKey: SecretEditor.Target?
    @FocusState private var composing: Bool
    @Environment(PushCenter.self) private var push

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 20) {
                ForEach(store.timeline.visibleItems) { item in
                    ChatRow(item: item).equatable()
                }
            }
            .scrollTargetLayout()
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
        .safeAreaBar(edge: .top) {
            if !loading, let banner = StageBanner.for(store.status) {
                StageBanner(banner: banner) { follow(banner.target) }
                    .padding(.horizontal, 12)
                    .padding(.top, 4)
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
        .animation(.snappy, value: store.status?.stage)
        .safeAreaBar(edge: .bottom) {
            VStack(spacing: 10) {
                if store.timeline.items.isEmpty, store.connection == .live {
                    Suggestions { send($0) }
                }
                Composer(
                    text: $draft, busy: store.busy, stopping: store.stopping, canSend: store.connection == .live,
                    focus: $composing,
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
        .sheet(isPresented: $showingMemory) {
            NavigationStack {
                MemoryView(client: client)
                    .toolbar { Button("Done") { showingMemory = false } }
            }
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
        .confirmationDialog(
            "This looks like a \(foundKey?.label ?? "key")",
            isPresented: Binding(get: { foundKey != nil }, set: { if !$0 { foundKey = nil } }),
            titleVisibility: .visible, presenting: foundKey
        ) { found in
            Button("Save it on your desktop") { savingKey = .new(found) }
            Button("Send it in the chat anyway", role: .destructive) { send(draft, checked: true) }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Messages go to the AI model and stay in the chat. A key saved on your desktop is given to commands by name, and the agent never sees it.")
        }
        .sheet(item: $savingKey) { target in
            NavigationStack {
                SecretEditor(client: client, target: target) { name in
                    // The message keeps its meaning, without the key.
                    if case .new(let found?) = target {
                        draft = draft.replacingOccurrences(of: found.value, with: "$\(name)")
                    }
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
        .environment(\.openTask, OpenTask { task in openConversation(task) })
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

    /// Closes Settings and scrolls to a task's conversation.
    private func openConversation(_ task: String) {
        showingSettings = false
        showingMemory = false
        let target = store.timeline.items.first { item in
            if case .work(let work) = item { work.task == task } else { false }
        }
        Task {
            try? await Task.sleep(for: .milliseconds(450))  // the sheet closing
            if let target {
                withAnimation(.snappy) { position.scrollTo(id: target.id, anchor: .top) }
            } else {
                store.notice = "That conversation is older than what the app has loaded."
            }
        }
    }

    /// Where the stage banner leads.
    private func follow(_ target: StageBanner.Target) {
        switch target {
        case .item(let id):
            withAnimation(.snappy) { position.scrollTo(id: id, anchor: .center) }
        case .task(let task):
            openConversation(task)
        case .lastAction:
            let action = store.timeline.items.last { if case .action = $0 { true } else { false } }
            if let action { withAnimation(.snappy) { position.scrollTo(id: action.id, anchor: .center) } }
        case .memory:
            showingMemory = true
        case .none:
            break
        }
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

    /// `checked`: the user chose to send a message with a key in it.
    private func send(_ text: String, checked: Bool = false) {
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        if !checked, let found = KeyDetector.find(in: text) {
            foundKey = found
            return
        }
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
    let stopping: Bool
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
                if busy && stopping {
                    // Pressed: feedback at once, until the task has ended.
                    ProgressView()
                        .controlSize(.small)
                        .tint(.black)
                        .frame(width: 38, height: 38)
                        .background(.white, in: .circle)
                        .accessibilityLabel("Stopping")
                } else if busy {
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
            .animation(.snappy(duration: 0.2), value: stopping)
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

// MARK: - Stage

/// What the desktop needs from the user, from agentd's `stage`: a slim banner
/// over the chat that leads to the card, the conversation or the memories.
/// Nothing shows while it's idle, working or only has a schedule coming.
private struct StageBanner: View {
    enum Target: Equatable {
        case item(String)
        case task(String)
        case lastAction
        case memory
        case none
    }

    struct Banner: Equatable {
        let text: String
        let icon: String
        let tint: Color
        let target: Target
    }

    let banner: Banner
    let tap: () -> Void

    var body: some View {
        Button(action: tap) {
            HStack(spacing: 8) {
                Image(systemName: banner.icon).foregroundStyle(banner.tint)
                Text(banner.text).font(.subheadline.weight(.medium)).lineLimit(1)
                Spacer(minLength: 4)
                Image(systemName: "chevron.right").font(.caption.weight(.bold)).foregroundStyle(.secondary)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .glassEffect(.regular.interactive(), in: .capsule)
    }

    static func `for`(_ status: AgentStatus?) -> Banner? {
        guard let status, let stage = status.stage else { return nil }
        let detail = status.stageDetail
        switch stage {
        case "needs_you/approval":
            let what = switch detail?["kind"]?.string {
            case "browser": "approve a change in the browser"
            case "schedule": "approve a schedule"
            default: "approve a command"
            }
            return Banner(
                text: "Needs you: \(what)", icon: "hand.raised.fill", tint: .orange,
                target: detail?["card"]?.string.map { .item("approval-\($0)") } ?? .none)
        case "needs_you/secret":
            return Banner(
                text: "Needs you: a key or token", icon: "key.fill", tint: .yellow,
                target: detail?["card"]?.string.map { .item("approval-\($0)") } ?? .none)
        case "needs_you/action":
            return Banner(text: "Needs you: action needed on the desktop", icon: "hand.raised.fill", tint: .orange, target: .lastAction)
        case "problem":
            if let run = detail?["run"]?.string {
                return Banner(text: "Something went wrong", icon: "exclamationmark.triangle.fill", tint: .red, target: .task(run))
            }
            return Banner(text: "Notifications or memory upkeep are failing", icon: "exclamationmark.triangle.fill", tint: .red, target: .none)
        case "review":
            let count = detail?["pending"]?.int ?? 0
            let items = count == 1 ? "1 memory item" : "\(count) memory items"
            return Banner(text: "Review \(items)", icon: "brain", tint: .orange, target: .memory)
        default:
            return nil
        }
    }
}
