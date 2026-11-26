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
                        .navigationSplitViewColumnWidth(min: 220, ideal: 260, max: 400)
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

/// The conversations, as ChatGPT's sidebar: New conversation at the top,
/// then each conversation's title, most recent first (what it's doing at the
/// end of the row), then the archived ones (folded), and Settings at the
/// foot. Right-click to rename, archive or delete.
private struct ConversationSidebar: View {
    let store: ChatStore
    let client: AgentClient
    let new: () -> Void
    @State private var showArchived = false
    @State private var renaming: AgentSession?
    @State private var deleting: AgentSession?
    /// The chat's Settings (also ⌘,): it shows over the chat.
    @FocusedValue(\.openSettings) private var settings

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 1) {
                    SidebarButton(title: "New conversation", icon: "square.and.pencil", action: new)
                        .help("New conversation (⌘N)")
                        .padding(.bottom, 14)
                    SidebarHeader(title: "Conversations")
                    ForEach(store.sessions) { session in
                        row(session)
                    }
                    archived
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
            }
            .scrollIndicators(.never)
            SidebarButton(title: "Settings", icon: "gearshape") { settings?.open() }
                .disabled(settings == nil)
                .help("Settings (⌘,)")
                .padding(.horizontal, 10)
                .padding(.vertical, 10)
        }
        .background(Theme.sidebar.ignoresSafeArea())
        .task(id: showArchived) {
            if showArchived { await store.loadArchived(client: client) }
        }
        .conversationEditing(store: store, client: client, renaming: $renaming, deleting: $deleting)
    }

    /// "Archived", folded: a tap shows them.
    @ViewBuilder private var archived: some View {
        Button {
            withAnimation(.snappy(duration: 0.2)) { showArchived.toggle() }
        } label: {
            HStack(spacing: 4) {
                Text("Archived")
                Image(systemName: "chevron.right")
                    .font(.system(size: 9, weight: .bold))
                    .rotationEffect(.degrees(showArchived ? 90 : 0))
            }
            .font(.app(.caption).weight(.medium))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 10)
            .padding(.top, 18)
            .padding(.bottom, 4)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        if showArchived {
            if let archived = store.archived {
                ForEach(archived) { session in
                    row(session)
                }
                if archived.isEmpty {
                    Text("Nothing archived")
                        .font(.app(.subheadline))
                        .foregroundStyle(.tertiary)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                }
            } else {
                ProgressView().controlSize(.small).padding(.horizontal, 10)
            }
        }
    }

    private func row(_ session: AgentSession) -> some View {
        SidebarRow(session: session, selected: store.selected == session.id) { store.select(session.id) }
            .conversationActions(
                session, rename: { renaming = session },
                archive: { Task { await store.setArchived(!session.archived, session.id, client: client) } },
                delete: { deleting = session })
    }
}

/// A small grey heading over a group of rows.
private struct SidebarHeader: View {
    let title: String

    var body: some View {
        Text(title)
            .font(.app(.caption).weight(.medium))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 10)
            .padding(.bottom, 4)
    }
}

/// A row that does something (New conversation, Settings): its icon and words.
private struct SidebarButton: View {
    let title: String
    let icon: String
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Label {
                Text(title)
            } icon: {
                Image(systemName: icon).foregroundStyle(.secondary)
            }
            .font(.app(.body))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(hovering ? Theme.hover : .clear, in: .rect(cornerRadius: 8))
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }
}

/// A conversation: its title, and at the end what it's doing (a spinner
/// while it works, then needs you › paused › queued). The one open is
/// shaded, as is the one under the pointer.
private struct SidebarRow: View {
    let session: AgentSession
    let selected: Bool
    let open: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: open) {
            HStack(spacing: 8) {
                Text(session.displayTitle)
                    .font(.app(.body))
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                state
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(selected ? Theme.selection : hovering ? Theme.hover : .clear, in: .rect(cornerRadius: 8))
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help("\(session.displayTitle) · \(ConversationRow.when(session.updated))")
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    @ViewBuilder private var state: some View {
        HStack(spacing: 6) {
            if session.state == .working {
                ProgressView()
                    .controlSize(.mini)
                    .accessibilityLabel("Working")
            }
            if session.needsYou != nil || session.state == .waiting {
                Circle().fill(.orange).frame(width: 7, height: 7)
                    .accessibilityLabel("Needs you")
            } else if session.state == .paused {
                Image(systemName: "pause.fill")
                    .font(.system(size: 9))
                    .foregroundStyle(.secondary)
                    .accessibilityLabel("Paused")
            } else if !session.queued.isEmpty {
                Text("\(session.queued.count)")
                    .font(.app(.caption2).weight(.semibold).monospacedDigit())
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background(Theme.chip, in: .capsule)
                    .accessibilityLabel("\(session.queued.count) queued")
            }
        }
        .fixedSize()
    }
}
