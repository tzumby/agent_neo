import AgentAPI
import ChatCore
import DesktopViewer
import SwiftUI

/// One conversation's chat. The home around it keeps the store in sync
/// (DesktopSync) and decides which conversation shows.
struct ChatView: View {
    let store: ChatStore
    let conversation: Conversation
    let client: AgentClient
    let desktopName: String
    let endpoint: DesktopEndpoint?
    let connectionKey: String

    @State private var showingSettings = false
    @State private var showingMemory = false
    @State private var showingUsage = false
    /// Stop was pressed with messages queued: it drops them, so it asks first.
    @State private var confirmingStop = false
    @State private var renaming: AgentSession?
    @State private var deleting: AgentSession?
    #if os(iOS)
        @State private var takeOver: TakeOverRequest?
        @Environment(\.dismiss) private var dismiss
    #else
        @Environment(\.openWindow) private var openWindow
        @Environment(DesktopSession.self) private var session
    #endif
    @State private var position = ScrollPosition(edge: .bottom)
    /// The spinner above the oldest message loaded is on screen.
    @State private var olderInView = false
    /// The oldest event of the chat on screen, to spot an older page going in (`keepPlace`).
    @State private var oldestShown: Int?
    #if os(macOS)
        /// Scrolled to the end: what the agent adds keeps it there (`followEnd`).
        @State private var atEnd = true
    #endif
    @State private var sent = 0
    /// A key in the message about to be sent: offer to save it instead.
    @State private var foundKey: KeyDetector.Found?
    @State private var savingKey: SecretEditor.Target?
    @FocusState private var composing: Bool
    @Environment(PushCenter.self) private var push
    @Environment(\.openConversation) private var openConversation

    var body: some View {
        presentations(over: chat)
            .sensoryFeedback(.impact(weight: .light), trigger: sent)
            .environment(\.chatStore, store)
            .environment(\.agentClient, client)
            .environment(\.takeOver, endpoint == nil ? nil : TakeOverAction { openDesktop(for: $0.id) })
            .environment(\.openTask, OpenTask { task in showTask(task) })
            // Under the splash until the first chat is in (the Mac's toolbar isn't covered by it).
            .toolbarVisibility(store.ready ? .automatic : .hidden, for: .appBar)
            // Its newest page; again with a new Tailscale session's client.
            .task(id: "\(conversation.id)|\(connectionKey)") { await store.open(conversation.id, client: client) }
            // A new Tailscale session restarts the search with its client.
            .task(id: "\(push.pendingCardID ?? "")|\(push.pendingSession ?? "")|\(store.selected)|\(connectionKey)") {
                await openNotificationCard()
            }
    }

    /// A notification was tapped: show its card. The home opens its
    /// conversation first. After the app was suspended the chat catches up
    /// first (`ChatStore.locate`), so a card that's still on its way isn't
    /// reported gone.
    private func openNotificationCard() async {
        guard let id = push.pendingCardID, push.pendingSession == nil, conversation.id == store.selected,
            !push.loggingOut
        else { return }
        let dismissingSheet = showingSettings || showingMemory || showingUsage
        showingSettings = false
        showingMemory = false
        showingUsage = false
        if dismissingSheet { try? await Task.sleep(for: .milliseconds(450)) }
        let found = await store.locate(id, in: conversation.id, client: client)
        guard !Task.isCancelled, push.pendingCardID == id else { return }
        switch found {
        case .waiting:
            withAnimation(.snappy) { position.scrollTo(id: id, anchor: .center) }
        case .settled:
            store.notice = "That request was already answered, or it expired."
        case .missing:
            store.notice = "That request isn't in the chat any more."
        case .unreachable:
            break  // the chat stays open at its end, where the card shows once the desktop answers
        }
        push.pendingCardID = nil
    }

    /// The conversation with its bars: the banners, the composer.
    private var chat: some View {
        messages
            .scrollDismissesKeyboard(.interactively)
            .overlay {
                if empty {
                    EmptyChat(desktopName: desktopName)
                } else if store.ready, !conversation.loaded {
                    // Another conversation, opened: its newest page is on its way.
                    ProgressView()
                }
            }
            .bar(.top) {
                if store.ready {
                    VStack(spacing: 8) {
                        if store.status?.budget?.paused == true {
                            BudgetBanner { showingUsage = true }
                                .transition(.move(edge: .top).combined(with: .opacity))
                        }
                        if let banner = StageBanner.for(store.status, here: conversation.id, title: title(of:)) {
                            StageBanner(banner: banner) { follow(banner) }
                                .transition(.move(edge: .top).combined(with: .opacity))
                        }
                    }
                    .frame(maxWidth: Theme.column)
                    .padding(.horizontal, 12)
                    .padding(.top, 4)
                }
            }
            .animation(.snappy, value: store.status?.stage)
            .animation(.snappy, value: store.status?.budget?.paused)
            .bar(.bottom) { bottomBar }
            .animation(.snappy, value: store.status?.needsModelKey)
            .background(Color.black)
            .inlineTitle()
            .toolbar { toolbar }
            #if os(macOS)
                // With a sidebar, the conversation's title; alone, the model menu is the title, as on the iPhone.
                .modifier(MacChatTitle(title: conversations ? title : nil))
            #endif
            .confirmationDialog(
                "Stop the task?", isPresented: $confirmingStop, titleVisibility: .visible
            ) {
                Button(conversation.queue.count == 1 ? "Stop and drop it" : "Stop and drop them", role: .destructive) {
                    stopNow()
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                let count = conversation.queue.count
                Text(
                    count == 1
                        ? "Stopping also drops the message queued after it."
                        : "Stopping also drops the \(count) messages queued after it.")
            }
    }

    /// The composer, or (archived) the way back.
    private var bottomBar: some View {
        VStack(spacing: 10) {
            if archived {
                ArchivedBar {
                    Task { await store.setArchived(false, conversation.id, client: client) }
                }
            } else {
                if let needed = store.status?.needsModelKey {
                    // No key for the chat model's provider: asked for here,
                    // so a new desktop's first message can go.
                    ModelKeyBanner(provider: needed, store: store, client: client)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                } else if empty {
                    Suggestions { send($0) }
                }
                Composer(
                    text: Bindable(conversation).draft, busy: conversation.busy, stopping: conversation.stopping,
                    canSend: store.connection == .live, queues: conversations, focus: $composing,
                    onSend: { send(conversation.draft) }, onStop: stop)
            }
        }
        .frame(maxWidth: Theme.column)
        .padding(.horizontal, 12)
        .padding(.bottom, 6)
    }

    /// Settings, Memory, Usage, the key dialog and editor, and (on the
    /// iPhone) the take-over view, over the chat. The store's notices show
    /// from the home (DesktopSync).
    private func presentations(over content: some View) -> some View {
        content
            .sheet(isPresented: $showingSettings) {
                NavigationStack { SettingsView(store: store, client: client) }
                    .macSheetSize()
            }
            .sheet(isPresented: $showingMemory) {
                NavigationStack {
                    MemoryView(client: client)
                        .toolbar { Button("Done") { showingMemory = false } }
                }
                .macSheetSize(width: 560, height: 640)
            }
            .sheet(isPresented: $showingUsage) {
                NavigationStack {
                    UsageView(store: store, client: client)
                        .toolbar { Button("Done") { showingUsage = false } }
                }
                .macSheetSize(width: 560, height: 700)
            }
            #if os(iOS)
                .fullScreenCover(item: $takeOver) { request in
                    if let endpoint {
                        TakeOverView(endpoint: endpoint, actionID: request.actionID, store: store, client: client)
                    } else {
                        // Tailscale is restarting (e.g. the app was in the background).
                        VStack(spacing: 16) {
                            ProgressView("Reconnecting to Tailscale…")
                            Button("Close") { takeOver = nil }.glassButton()
                        }
                    }
                }
            #else
                // ⌘, opens Settings while this window is in front.
                .focusedSceneValue(\.openSettings, OpenSettings { showingSettings = true })
            #endif
            .confirmationDialog(
                "This looks like a \(foundKey?.label ?? "key")",
                isPresented: Binding(get: { foundKey != nil }, set: { if !$0 { foundKey = nil } }),
                titleVisibility: .visible, presenting: foundKey
            ) { found in
                Button("Save it on your desktop") { savingKey = .new(found) }
                Button("Send it in the chat anyway", role: .destructive) { send(conversation.draft, checked: true) }
                Button("Cancel", role: .cancel) {}
            } message: { _ in
                Text("Messages go to the AI model and stay in the chat. A key saved on your desktop is given to commands by name, and the agent never sees it.")
            }
            .sheet(item: $savingKey) { target in
                NavigationStack {
                    SecretEditor(client: client, target: target) { name in
                        // The message keeps its meaning, without the key.
                        if case .new(let found?) = target {
                            conversation.draft = conversation.draft.replacingOccurrences(of: found.value, with: "$\(name)")
                        }
                    }
                }
                .macSheetSize(height: 420)
            }
            .conversationEditing(store: store, client: client, renaming: $renaming, deleting: $deleting)
    }

    /// The conversation, newest at the bottom, its queued messages under it;
    /// older pages load as the user scrolls up to the oldest one loaded. A
    /// VStack, not a lazy one: only the pages loaded are in it, and its exact
    /// height keeps the place when a page goes in above (a lazy stack only
    /// estimates rows not yet shown).
    private var messages: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                if conversation.hasOlder {
                    ProgressView()
                        .frame(maxWidth: .infinity)
                        .onScrollVisibilityChange(threshold: 0.01) { olderInView = $0 }
                        .task(id: OlderPage(inView: olderInView, oldest: conversation.oldestLoaded)) {
                            while olderInView, !Task.isCancelled, conversation.hasOlder,
                                await !store.loadOlder(conversation, client: client)
                            {
                                try? await Task.sleep(for: .seconds(3))  // offline: try again while in view
                            }
                        }
                }
                ForEach(conversation.timeline.visibleItems) { item in
                    ChatRow(item: item).equatable()
                }
                if conversation.busy {
                    WorkingIndicator(since: conversation.stopping ? nil : conversation.timeline.workingSince)
                }
                let queue = conversation.queue
                ForEach(Array(queue.enumerated()), id: \.element.id) { index, message in
                    QueuedBubble(message: message, position: queue.count > 1 ? index + 1 : nil) {
                        Task { await store.remove(message, from: conversation.id, client: client) }
                    }
                    .transition(.opacity.combined(with: .scale(scale: 0.96, anchor: .bottomTrailing)))
                }
            }
            .animation(.snappy(duration: 0.25), value: conversation.queue.map(\.id))
            .scrollTargetLayout()
            // A VStack is only as wide as its rows: without any (an empty
            // chat), the scroll view and the empty state over it shrank.
            .frame(maxWidth: Theme.column, alignment: .leading)
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 18)
            .padding(.vertical, 12)
        }
        .scrollPosition($position)
        // Start at the newest message and stay there as the agent adds more.
        .defaultScrollAnchor(.bottom, for: .initialOffset)
        .defaultScrollAnchor(.bottom, for: .sizeChanges)
        .onScrollGeometryChange(for: ScrollGeometry.self, of: { $0 }) { old, new in
            #if os(macOS)
                let olderPage = conversation.oldestLoaded != oldestShown
            #endif
            keepPlace(old, new)
            #if os(macOS)
                followEnd(old, new, olderPage: olderPage)
            #endif
        }
        #if os(macOS)
            // The Mac's scroll view starts at the top when the first page comes
            // after it appeared (empty): go to the end, under the splash.
            .onChange(of: conversation.loaded) { _, loaded in
                if loaded { position.scrollTo(edge: .bottom) }
            }
        #endif
    }

    /// An older page went in above. Scrolled up to the top, the scroll view
    /// keeps its offset, which would show the new page's first messages (and
    /// its spinner, loading the next page, and the next): move down by the
    /// height that went in, so what was on screen stays put. (Runs as the
    /// content's size changes, which comes before an onChange of the store.)
    private func keepPlace(_ old: ScrollGeometry, _ new: ScrollGeometry) {
        let oldest = conversation.oldestLoaded
        guard oldest != oldestShown else { return }
        defer { oldestShown = oldest }
        let added = new.contentSize.height - old.contentSize.height
        guard let shown = oldestShown, let oldest, oldest < shown, added > 0,
            // Not moved down by the scroll view itself (scrolled to the end, it
            // keeps the end); a finger still dragging moves only a little.
            new.contentOffset.y - old.contentOffset.y < added / 2
        else { return }
        // scrollTo(y:) counts from the content's top; contentOffset includes the top inset (the bars).
        position.scrollTo(y: new.contentOffset.y + added + new.contentInsets.top)
    }

    #if os(macOS)
        /// The Mac's scroll view doesn't keep its end as the content grows
        /// (`defaultScrollAnchor(.bottom, for: .sizeChanges)` does nothing
        /// there), so new messages went in below the window. Scrolled to the
        /// end, it goes back there; scrolled up to read, it stays put. Only
        /// scrolling decides which: the content's height unchanged and the
        /// offset moved (other updates come between growing and catching up).
        private func followEnd(_ old: ScrollGeometry, _ new: ScrollGeometry, olderPage: Bool) {
            if new.contentSize.height != old.contentSize.height {
                if atEnd, !olderPage { position.scrollTo(edge: .bottom) }
            } else if new.contentOffset.y != old.contentOffset.y {
                // Within a few lines of the end (the composer covers the last ones).
                atEnd = new.visibleRect.maxY >= new.contentSize.height - 80
            }
        }
    #endif

    /// Closes Settings and scrolls to a task's conversation (the message that
    /// started it), loading older chat until it's there. A task of another
    /// conversation (one that's loaded) opens that one.
    private func showTask(_ task: String) {
        showingSettings = false
        showingMemory = false
        Task {
            try? await Task.sleep(for: .milliseconds(450))  // the sheet closing
            var target = prompt(of: task)
            if target == nil, let other = store.conversation(startingTask: task), let openConversation {
                openConversation.open(other)
                return
            }
            while target == nil, await store.loadOlder(conversation, client: client) {
                target = prompt(of: task)
            }
            if let target {
                withAnimation(.snappy) { position.scrollTo(id: target, anchor: .top) }
            } else {
                store.notice = "Couldn't find that conversation in the chat."
            }
        }
    }

    /// The id of the row where a task started: its message, or its report.
    private func prompt(of task: String) -> String? {
        conversation.timeline.itemID(startingTask: task)
    }

    /// Where the stage banner leads. A banner about another conversation
    /// opens it (and its card, the way a notification does).
    private func follow(_ banner: StageBanner.Banner) {
        if let elsewhere = banner.elsewhere {
            if case .item(let id) = banner.target { push.pendingCardID = id }
            push.pendingSession = elsewhere.id
            return
        }
        switch banner.target {
        case .item(let id):
            withAnimation(.snappy) { position.scrollTo(id: id, anchor: .center) }
        case .task(let task):
            showTask(task)
        case .lastAction:
            let action = conversation.timeline.items.last { if case .action = $0 { true } else { false } }
            if let action { withAnimation(.snappy) { position.scrollTo(id: action.id, anchor: .center) } }
        case .memory:
            showingMemory = true
        case .none:
            break
        }
    }

    /// The desktop has conversations: the list is shown, and sending while
    /// it works queues.
    private var conversations: Bool { store.conversationsAvailable == true }

    /// What agentd last said about this conversation.
    private var listing: AgentSession? { store.session(conversation.id) }

    /// The conversation's title.
    private var title: String { listing?.displayTitle ?? "Conversation" }

    private func title(of id: String) -> String? { store.session(id)?.displayTitle }

    /// Archived: it can be read, and brought back to write in it.
    private var archived: Bool { listing?.archived == true }

    /// Nothing said yet in this conversation.
    private var empty: Bool {
        conversation.loaded && conversation.timeline.items.isEmpty && conversation.queue.isEmpty
            && store.connection == .live
    }

    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .barLeading) {
            #if os(iOS)
                // With conversations, Settings is on the list (the back button is here).
                if !conversations { settingsButton }
            #else
                settingsButton
            #endif
        }
        ToolbarItem(placement: .principal) {
            #if os(iOS)
                ModelMenu(
                    store: store, client: client, desktopName: desktopName,
                    conversationTitle: conversations ? title : nil, rename: renameAction, archive: archiveAction)
            #else
                ModelMenu(store: store, client: client, desktopName: desktopName)
            #endif
        }
        ToolbarItemGroup(placement: .barTrailing) {
            Button("Your desktop", systemImage: "display") { openDesktop(for: nil) }
                .disabled(endpoint == nil)
            #if os(iOS)
                if conversations, let openConversation {
                    Button("New conversation", systemImage: "square.and.pencil") { openConversation.new() }
                }
            #endif
        }
    }

    /// The title menu renames or archives a listed conversation.
    private var renameAction: (() -> Void)? {
        guard conversations, !archived, let listing else { return nil }
        return { renaming = listing }
    }

    private var archiveAction: (() -> Void)? {
        guard conversations, !archived, listing != nil else { return nil }
        return archive
    }

    private var settingsButton: some View {
        Button("Settings", systemImage: "slider.horizontal.3") { showingSettings = true }
    }

    /// Archives it from its title menu, and goes back to the list.
    private func archive() {
        let id = conversation.id
        Task {
            await store.setArchived(true, id, client: client)
            #if os(iOS)
                if store.session(id)?.archived == true { dismiss() }
            #endif
        }
    }

    /// Opens the take-over view, for an Action needed card or just to look:
    /// full screen on the iPhone, its own window on the Mac.
    private func openDesktop(for actionID: String?) {
        #if os(iOS)
            takeOver = TakeOverRequest(actionID: actionID)
        #else
            session.takeOverAction = actionID
            openWindow(id: DesktopSession.desktopWindow)
        #endif
    }

    /// Stop drops the queued messages too: with some, it asks first.
    private func stop() {
        if conversation.queue.isEmpty { stopNow() } else { confirmingStop = true }
    }

    private func stopNow() {
        let id = conversation.id
        Task { await store.stop(id, client: client) }
    }

    /// `checked`: the user chose to send a message with a key in it.
    private func send(_ text: String, checked: Bool = false) {
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        if !checked, let found = KeyDetector.find(in: text) {
            foundKey = found
            return
        }
        conversation.draft = ""
        sent += 1
        #if os(macOS)
            // At once: offsets seen mid-animation would read as scrolled up.
            atEnd = true
            position.scrollTo(edge: .bottom)
        #else
            withAnimation(.snappy) { position.scrollTo(edge: .bottom) }
        #endif
        let id = conversation.id
        Task { await store.send(text, to: id, client: client) }
    }
}

/// When to (re)load the page before the oldest one: the spinner came into
/// view, or a page came and it's still in view.
private struct OlderPage: Equatable {
    let inView: Bool
    let oldest: Int?
}

#if os(macOS)
    /// The conversation's title in the window's toolbar, or none (the model
    /// menu is the title).
    private struct MacChatTitle: ViewModifier {
        let title: String?

        func body(content: Content) -> some View {
            if let title {
                content.navigationTitle(title)
            } else {
                content.toolbar(removing: .title)
            }
        }
    }
#endif

// MARK: - Composer

/// The floating input: a glass field that grows to six lines, with a white
/// send button that turns into Stop while the agent works. On a desktop with
/// conversations, typing while it works brings Send back beside Stop:
/// sending then queues the message.
private struct Composer: View {
    @Binding var text: String
    let busy: Bool
    let stopping: Bool
    let canSend: Bool
    /// Sending while it works queues the message (an older desktop refuses it).
    let queues: Bool
    var focus: FocusState<Bool>.Binding
    let onSend: () -> Void
    let onStop: () -> Void

    private var empty: Bool { text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }

    /// Send shows when idle, and while it works once there's something to queue.
    private var showsSend: Bool { !busy || (queues && !empty) }

    var body: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextField(busy && queues ? "Queue a message" : "Ask your desktop", text: $text, axis: .vertical)
                .lineLimit(1...6)
                .focused(focus)
                #if os(macOS)
                    // Return sends, as in Messages (⌥Return starts a new line).
                    .onSubmit { if showsSend, canSend { onSend() } }
                    .textFieldStyle(.plain)
                #endif
                .padding(.leading, 18)
                .padding(.vertical, 14)
            HStack(spacing: 6) {
                if busy && stopping {
                    // Pressed: feedback at once, until the task has ended.
                    ProgressView()
                        .controlSize(.small)
                        .tint(showsSend ? .white : .black)
                        .frame(width: 38, height: 38)
                        .background(showsSend ? Color.white.opacity(0.14) : .white, in: .circle)
                        .accessibilityLabel("Stopping")
                        .transition(.scale.combined(with: .opacity))
                } else if busy {
                    // Beside Send, Stop steps back to the same circle in grey.
                    Button(action: onStop) {
                        Image(systemName: "stop.fill").font(.system(size: 14, weight: .bold))
                    }
                    .buttonStyle(SendButtonStyle(prominent: !showsSend))
                    .accessibilityLabel("Stop")
                    .transition(.scale.combined(with: .opacity))
                }
                if showsSend {
                    Button(action: onSend) {
                        Image(systemName: "arrow.up").font(.system(size: 16, weight: .bold))
                    }
                    .buttonStyle(SendButtonStyle())
                    .accessibilityLabel(busy ? "Queue" : "Send")
                    .disabled(empty || !canSend)
                    .transition(.scale.combined(with: .opacity))
                }
            }
            .padding(6)
            .animation(.snappy(duration: 0.2), value: busy)
            .animation(.snappy(duration: 0.2), value: stopping)
            .animation(.snappy(duration: 0.2), value: showsSend)
        }
        .glass(.rect(cornerRadius: 26), interactive: true)
    }
}

/// A white circle with a black glyph (or, not prominent, a grey one with a
/// white glyph), dimmed when disabled; shrinks on press.
private struct SendButtonStyle: ButtonStyle {
    var prominent = true
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(prominent ? .black : .white)
            .frame(width: 38, height: 38)
            .background(
                prominent ? Color.white.opacity(isEnabled ? 1 : 0.25) : Color.white.opacity(0.14), in: .circle
            )
            .contentShape(.circle)
            .scaleEffect(configuration.isPressed ? 0.9 : 1)
            .animation(.snappy(duration: 0.15), value: configuration.isPressed)
    }
}

// MARK: - Queued messages

/// A message waiting its turn: grey and dashed, under the chat, with its
/// place in line when there's more than one, and a ✕ to take it back.
private struct QueuedBubble: View {
    let message: QueuedMessage
    /// 1 is next; nil when it's the only one.
    let position: Int?
    let remove: () -> Void

    var body: some View {
        VStack(alignment: .trailing, spacing: 6) {
            HStack(spacing: 4) {
                Image(systemName: "clock")
                Text(position.map { "Queued · \($0)" } ?? "Queued")
            }
            .font(.app(.caption).weight(.medium))
            .foregroundStyle(.secondary)
            .padding(.trailing, 6)
            HStack(alignment: .top, spacing: 10) {
                Text(message.text)
                    .foregroundStyle(.secondary)
                    .lineLimit(8)
                    .fixedSize(horizontal: false, vertical: true)
                Button(action: remove) {
                    Image(systemName: "xmark")
                        .font(.system(size: 10, weight: .bold))
                        .foregroundStyle(.secondary)
                        .frame(width: 22, height: 22)
                        .background(Color.white.opacity(0.1), in: .circle)
                        .contentShape(.circle.inset(by: -8))
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Remove from the queue")
                .help("Remove from the queue")
            }
            .padding(.leading, 16)
            .padding(.trailing, 10)
            .padding(.vertical, 11)
            .background(Theme.userBubble.opacity(0.4), in: .rect(cornerRadius: 22))
            .overlay(
                RoundedRectangle(cornerRadius: 22)
                    .strokeBorder(Color.white.opacity(0.2), style: StrokeStyle(lineWidth: 1, dash: [4, 3]))
            )
        }
        .frame(maxWidth: .infinity, alignment: .trailing)
        .padding(.leading, 56)
        .accessibilityElement(children: .combine)
        .contextMenu {
            Button("Copy", systemImage: "doc.on.doc") { Clipboard.copy(message.text) }
            Button("Remove from the queue", systemImage: "xmark", role: .destructive, action: remove)
        }
    }
}

// MARK: - Banners and bars

/// This month's budget is used up: new messages wait. Leads to Usage.
private struct BudgetBanner: View {
    let openUsage: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "gauge.with.dots.needle.100percent")
                .font(.app(.title3))
                .foregroundStyle(.yellow)
            // Wraps at the width it gets. Not fixedSize: the Mac's split view
            // measures the chat's minimum height at a sliver of width, where
            // the text would stand a thousand points tall and push the
            // composer out of the window.
            Text("This month's budget is used up. New messages wait until you raise it.")
                .font(.app(.subheadline).weight(.medium))
                .lineLimit(1...4)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button("Usage", action: openUsage)
                .font(.app(.subheadline).weight(.semibold))
                .glassButton()
        }
        .padding(.leading, 14)
        .padding(.trailing, 8)
        .padding(.vertical, 8)
        .glass(.rect(cornerRadius: 22))
        .accessibilityElement(children: .contain)
    }
}

/// In place of the composer while the conversation is archived.
private struct ArchivedBar: View {
    let unarchive: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            Label("Archived", systemImage: "archivebox")
                .font(.app(.subheadline).weight(.medium))
                .foregroundStyle(.secondary)
            Spacer()
            Button("Unarchive", action: unarchive)
                .font(.app(.subheadline).weight(.semibold))
                .glassButton()
        }
        .padding(.leading, 18)
        .padding(.trailing, 8)
        .padding(.vertical, 8)
        .glass(.capsule)
    }
}

// MARK: - Title: model and connection

/// The centre of the bar, as in Grok: the model, with a menu to switch it,
/// and a dot for the connection to the desktop. On the iPhone with
/// conversations, the conversation's title above it, and the menu renames or
/// archives it too.
private struct ModelMenu: View {
    let store: ChatStore
    let client: AgentClient
    let desktopName: String
    var conversationTitle: String? = nil
    var rename: (() -> Void)? = nil
    var archive: (() -> Void)? = nil

    var body: some View {
        #if os(macOS)
            // A Mac toolbar menu shows only its label's text and first image:
            // the dot goes beside it, and the menu has the system's arrow.
            HStack(spacing: 6) {
                Circle().fill(dotColor).frame(width: 7, height: 7)
                Menu(title) { items }
                    .menuStyle(.borderlessButton)
                    .fixedSize()
            }
            .accessibilityElement(children: .contain)
            .accessibilityLabel("Model and connection")
        #else
            Menu {
                items
            } label: {
                label
                    .padding(.horizontal, 4)
                    .contentShape(.rect)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(conversationTitle.map { "\($0), model and connection" } ?? "Model and connection")
        #endif
    }

    #if os(iOS)
        @ViewBuilder private var label: some View {
            if let conversationTitle {
                VStack(spacing: 1) {
                    Text(conversationTitle)
                        .font(.app(.headline))
                        .lineLimit(1)
                    HStack(spacing: 4) {
                        Circle().fill(dotColor).frame(width: 6, height: 6)
                        Text(title).lineLimit(1)
                        Image(systemName: "chevron.down").font(.app(.caption2).weight(.bold))
                    }
                    .font(.app(.caption))
                    .foregroundStyle(.secondary)
                }
                .frame(maxWidth: 220)
            } else {
                HStack(spacing: 6) {
                    Circle().fill(dotColor).frame(width: 7, height: 7)
                    Text(title)
                        .font(.app(.headline))
                        .lineLimit(1)
                    Image(systemName: "chevron.down").font(.app(.caption2).weight(.bold)).foregroundStyle(.secondary)
                }
            }
        }
    #endif

    private var title: String {
        store.status.map { shortName($0.settings.model) } ?? desktopName
    }

    @ViewBuilder private var items: some View {
        if rename != nil || archive != nil {
            Section {
                if let rename { Button("Rename…", systemImage: "pencil", action: rename) }
                if let archive { Button("Archive", systemImage: "archivebox", action: archive) }
            }
        }
        if let status = store.status {
            Picker("Model", selection: modelBinding(status)) {
                ForEach(status.knownModels, id: \.self) { Text(shortName($0)).tag($0) }
            }
            Toggle("Browser actions", isOn: actionsBinding(status))
        }
        Section(desktopName) {
            Text(connectionText)
        }
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
        ModelName.display(model)
    }

    private func modelBinding(_ status: AgentStatus) -> Binding<String> {
        Binding(
            get: { status.settings.model },
            set: { model in
                Task {
                    if let problem = await store.updateSettings(model: model, client: client) { store.notice = problem }
                }
            })
    }

    private func actionsBinding(_ status: AgentStatus) -> Binding<Bool> {
        Binding(
            get: { status.settings.allowActions },
            set: { allow in
                Task {
                    if let problem = await store.updateSettings(allowActions: allow, client: client) {
                        store.notice = problem
                    }
                }
            })
    }
}

// MARK: - Working

/// Three dots that pulse while the agent works, and how long it has been at
/// it ("Working · 24 min", from a minute on): all the chat shows of it.
private struct WorkingIndicator: View {
    /// When the running task started (nil while stopping, or unknown).
    let since: Date?

    var body: some View {
        TimelineView(.animation) { context in
            let time = context.date.timeIntervalSinceReferenceDate
            HStack(spacing: 10) {
                HStack(spacing: 5) {
                    ForEach(0..<3, id: \.self) { dot in
                        Circle()
                            .frame(width: 7, height: 7)
                            .opacity(0.25 + 0.6 * pulse(time - Double(dot) * 0.2))
                    }
                }
                if let working = ChatTimeline.working(since: since, now: context.date) {
                    Text("Working · \(working)").font(.app(.footnote)).monospacedDigit()
                }
            }
        }
        .foregroundStyle(.secondary)
        .padding(.vertical, 6)
        .accessibilityElement()
        .accessibilityLabel(ChatTimeline.working(since: since).map { "Working for \($0)" } ?? "Working")
    }

    /// 0…1 and back, once every 1.2 s.
    private func pulse(_ time: Double) -> Double {
        (sin(time * 2 * .pi / 1.2) + 1) / 2
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
                .glass(.circle)
            Text("What should your desktop do?")
                .font(.app(.title2).weight(.semibold))
                .multilineTextAlignment(.center)
            Text(desktopName)
                .font(.app(.subheadline))
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
            GlassGroup(spacing: 8) {
                HStack(spacing: 8) {
                    ForEach(items, id: \.1) { icon, text in
                        Button { pick(text) } label: {
                            Label(text, systemImage: icon).font(.app(.subheadline))
                        }
                        .glassButton()
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
/// About another conversation, it names that one and opens it.
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
        /// The conversation it's about, when it isn't the one on screen.
        var elsewhere: Elsewhere?
    }

    struct Elsewhere: Equatable {
        let id: String
        let title: String
    }

    let banner: Banner
    let tap: () -> Void

    var body: some View {
        Button(action: tap) {
            HStack(spacing: 8) {
                Image(systemName: banner.icon).foregroundStyle(banner.tint)
                VStack(alignment: .leading, spacing: 1) {
                    Text(banner.text).font(.app(.subheadline).weight(.medium)).lineLimit(1)
                    if let elsewhere = banner.elsewhere {
                        Text("In “\(elsewhere.title)”")
                            .font(.app(.caption))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                }
                Spacer(minLength: 4)
                Image(systemName: "chevron.right").font(.app(.caption).weight(.bold)).foregroundStyle(.secondary)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .glass(.capsule, interactive: true)
    }

    /// `here`: the conversation on screen; `title`: another one's title.
    static func `for`(_ status: AgentStatus?, here: String, title: (String) -> String?) -> Banner? {
        guard var banner = self.for(status) else { return nil }
        if let session = status?.stageDetail?["session"]?.string, session != here {
            banner.elsewhere = Elsewhere(id: session, title: title(session) ?? "another conversation")
        }
        return banner
    }

    static func `for`(_ status: AgentStatus?) -> Banner? {
        guard let status, let stage = status.stage else { return nil }
        let detail = status.stageDetail
        switch stage {
        case "needs_you/approval":
            let what = switch detail?["kind"]?.string {
            case "browser": "approve a change in the browser"
            case "app": "approve a change in an app"
            case "install": "approve an app install"
            case "schedule": "approve a schedule"
            case "access": "allow a site"
            case "stuck": "your task looks stuck"
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
        default:
            return nil
        }
    }
}
