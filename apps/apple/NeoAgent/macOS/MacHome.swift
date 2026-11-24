import AgentAPI
import ChatCore
import SwiftUI

/// The Mac: the conversations in a sidebar beside the chat (on opening, the
/// one used last). A desktop from before conversations is its chat alone,
/// as before.
struct MacHome: View {
    let home: DesktopHomeContent
    @Environment(PushCenter.self) private var push

    private var store: ChatStore { home.store }

    var body: some View {
        Group {
            if store.conversationsAvailable == true {
                NavigationSplitView {
                    ConversationSidebar(store: store, client: home.client) { newConversation() }
                        .navigationSplitViewColumnWidth(min: 220, ideal: 270, max: 400)
                } detail: {
                    // Its own view per conversation: the scroll position and the rest start afresh.
                    NavigationStack { home.chat(store.selected) }
                        .id(store.selected)
                        // The split view measures the chat's minimum height at its minimum width.
                        .navigationSplitViewColumnWidth(min: 360, ideal: 720)
                }
            } else {
                // An older desktop, or not known yet (under the splash).
                NavigationStack { home.chat(store.selected) }
            }
        }
        .environment(\.openConversation, OpenConversation(open: { store.select($0) }, new: { newConversation() }))
        // ⌘N.
        .focusedSceneValue(
            \.newConversation, NewConversation(enabled: store.conversationsAvailable == true) { newConversation() })
        .modifier(DesktopSync(home: home))
        // A notification (or a banner about another conversation) opens its conversation.
        .onChange(of: push.pendingSession, initial: true) { _, id in
            guard let id else { return }
            push.pendingSession = nil
            store.select(id)
        }
    }

    private func newConversation() {
        Task { await store.newConversation(client: home.client) }
    }
}

/// ⌘N: a new conversation, on a desktop that has them.
struct NewConversation {
    let enabled: Bool
    let start: () -> Void
}

extension FocusedValues {
    @Entry var newConversation: NewConversation?
}

/// The conversations, most recent first, then the archived ones (folded).
/// Right-click (or swipe) to rename, archive or delete.
private struct ConversationSidebar: View {
    let store: ChatStore
    let client: AgentClient
    let new: () -> Void
    @State private var showArchived = false
    @State private var renaming: AgentSession?
    @State private var deleting: AgentSession?

    var body: some View {
        List(selection: Binding(get: { store.selected }, set: { if let id = $0 { store.select(id) } })) {
            Section("Conversations") {
                ForEach(store.sessions) { session in
                    row(session)
                }
            }
            Section("Archived", isExpanded: $showArchived) {
                if let archived = store.archived {
                    ForEach(archived) { session in
                        row(session)
                    }
                    if archived.isEmpty {
                        Text("Nothing archived")
                            .font(.app(.subheadline))
                            .foregroundStyle(.tertiary)
                    }
                } else {
                    ProgressView().controlSize(.small)
                }
            }
        }
        .listStyle(.sidebar)
        .task(id: showArchived) {
            if showArchived { await store.loadArchived(client: client) }
        }
        .toolbar {
            ToolbarItem {
                Button("New Conversation", systemImage: "square.and.pencil", action: new)
                    .help("New conversation (⌘N)")
            }
        }
        .conversationEditing(store: store, client: client, renaming: $renaming, deleting: $deleting)
    }

    private func row(_ session: AgentSession) -> some View {
        ConversationRow(session: session, compact: true)
            .tag(session.id)
            .conversationActions(
                session, rename: { renaming = session },
                archive: { Task { await store.setArchived(!session.archived, session.id, client: client) } },
                delete: { deleting = session })
    }
}
