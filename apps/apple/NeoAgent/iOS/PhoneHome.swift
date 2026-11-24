import AgentAPI
import ChatCore
import SwiftUI

/// The iPhone: the conversations are a list screen, with a conversation's
/// chat pushed over it (on opening, the one used last). A desktop from
/// before conversations is its chat alone, as before.
struct PhoneHome: View {
    let home: DesktopHomeContent
    /// Where it opens: nil for the conversation used last.
    var initialPath: [ConversationRoute]?
    @Environment(PushCenter.self) private var push
    @State private var path: [ConversationRoute] = []

    private var store: ChatStore { home.store }

    var body: some View {
        NavigationStack(path: $path) {
            Group {
                if store.conversationsAvailable == true {
                    ConversationList(store: store, client: home.client, desktopName: home.desktopName)
                } else {
                    // An older desktop, or not known yet (under the splash).
                    home.chat(store.selected)
                }
            }
            .navigationDestination(for: ConversationRoute.self) { route in
                switch route {
                case .chat(let id): home.chat(id)
                case .archived: ArchivedConversations(store: store, client: home.client)
                }
            }
        }
        .environment(\.openConversation, OpenConversation(open: { show($0) }, new: { newConversation() }))
        .modifier(DesktopSync(home: home))
        // Opening the app shows the conversation used last, over the list (under the splash).
        .onChange(of: store.conversationsAvailable, initial: true) { _, available in
            guard available == true, path.isEmpty else { return }
            var instantly = Transaction()
            instantly.disablesAnimations = true
            withTransaction(instantly) { path = initialPath ?? [.chat(store.selected)] }
        }
        .onChange(of: path) { _, path in
            if case .chat(let id)? = path.last { store.select(id) }
        }
        // A notification (or a banner about another conversation) opens its conversation.
        .onChange(of: push.pendingSession, initial: true) { _, id in
            guard let id else { return }
            push.pendingSession = nil
            show(id)
        }
    }

    /// Shows a conversation's chat over the list.
    private func show(_ id: String) {
        store.select(id)
        guard store.conversationsAvailable == true, path != [.chat(id)] else { return }
        path = [.chat(id)]
    }

    private func newConversation() {
        Task {
            if let id = await store.newConversation(client: home.client) { show(id) }
        }
    }
}

/// The conversations, most recent first: each with its last message and what
/// it's doing. Swipe to archive or delete; touch and hold to rename.
private struct ConversationList: View {
    let store: ChatStore
    let client: AgentClient
    let desktopName: String
    @Environment(\.openConversation) private var openConversation
    @State private var showingSettings = false
    @State private var renaming: AgentSession?
    @State private var deleting: AgentSession?

    var body: some View {
        List {
            Section {
                ForEach(store.sessions) { session in
                    NavigationLink(value: ConversationRoute.chat(session.id)) {
                        ConversationRow(session: session)
                    }
                    .conversationActions(
                        session, rename: { renaming = session },
                        archive: { Task { await store.setArchived(true, session.id, client: client) } },
                        delete: { deleting = session })
                }
            }
            Section {
                NavigationLink(value: ConversationRoute.archived) {
                    Label("Archived", systemImage: "archivebox")
                        .foregroundStyle(.secondary)
                }
            } footer: {
                if store.runningLimit > 1 {
                    Text("Up to \(store.runningLimit) conversations work at the same time; messages to others wait their turn.")
                }
            }
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
        .background(Color.black)
        .navigationTitle("Conversations")
        .navigationSubtitle(subtitle)
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                Button("Settings", systemImage: "slider.horizontal.3") { showingSettings = true }
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button("New conversation", systemImage: "square.and.pencil") { openConversation?.new() }
            }
        }
        .refreshable { try? await store.refreshList(client: client) }
        .sheet(isPresented: $showingSettings) {
            NavigationStack { SettingsView(store: store, client: client) }
        }
        .conversationEditing(store: store, client: client, renaming: $renaming, deleting: $deleting)
    }

    /// The desktop, and whether it's connected.
    private var subtitle: String {
        switch store.connection {
        case .live: desktopName
        case .connecting: "Connecting to \(desktopName)…"
        case .offline: "\(desktopName) is offline"
        }
    }
}
