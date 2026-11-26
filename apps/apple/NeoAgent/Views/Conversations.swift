import AgentAPI
import ChatCore
import SwiftUI

// The pieces of the conversation list both apps share: the iPhone's list
// screen (iOS/PhoneHome.swift) and the Mac's sidebar (macOS/MacHome.swift),
// which has rows of its own (titles only).

/// A conversation in the list: its title and when, the last message's first
/// line, and what it's doing (working, needs you, queued, paused).
struct ConversationRow: View {
    let session: AgentSession

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(session.displayTitle)
                    .font(.app(.body).weight(.medium))
                    .lineLimit(1)
                Spacer(minLength: 4)
                Text(Self.when(session.updated))
                    .font(.app(.footnote))
                    .monospacedDigit()
                    .foregroundStyle(.tertiary)
            }
            HStack(alignment: .center, spacing: 8) {
                preview
                    .font(.app(.subheadline))
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .leading)
                ConversationBadge(session: session)
            }
        }
        .padding(.vertical, 7)
        .accessibilityElement(children: .combine)
    }

    /// "You: Plan the trip…", or the agent's answer: its first line, as
    /// plain text (Markdown's marks dropped, as Messages shows a preview).
    private var preview: Text {
        guard let last = session.last else {
            return Text("No messages yet").foregroundStyle(.tertiary)
        }
        var line =
            last.text.split(whereSeparator: \.isNewline).first.map(String.init)?
            .trimmingCharacters(in: .whitespaces) ?? ""
        if case .paragraph(let attributed)? = MarkdownBlock.parse(line).first {
            line = String(attributed.characters)
        }
        return Text(last.role == "user" ? "You: \(line)" : line).foregroundStyle(.secondary)
    }

    /// Today "14:05", then "Yesterday", the weekday, and a date.
    static func when(_ date: Date, now: Date = .now) -> String {
        let calendar = Calendar.current
        if calendar.isDateInToday(date) { return date.formatted(date: .omitted, time: .shortened) }
        if calendar.isDateInYesterday(date) { return "Yesterday" }
        if let days = calendar.dateComponents([.day], from: date, to: now).day, days < 7 {
            return date.formatted(.dateTime.weekday(.abbreviated))
        }
        if calendar.isDate(date, equalTo: now, toGranularity: .year) {
            return date.formatted(.dateTime.month(.abbreviated).day())
        }
        return date.formatted(date: .numeric, time: .omitted)
    }
}

/// What a conversation is doing, at a glance: a spinner while it works, then
/// the one thing to know (needs you › paused › queued).
struct ConversationBadge: View {
    let session: AgentSession

    var body: some View {
        HStack(spacing: 6) {
            if session.state == .working {
                ProgressView()
                    .controlSize(.small)
                    .accessibilityLabel("Working")
            }
            if session.needsYou != nil || session.state == .waiting {
                Pill(text: "Needs you", icon: "hand.raised.fill", tint: .orange)
            } else if session.state == .paused {
                Pill(text: "Paused", icon: "pause.fill", tint: Theme.amber)
            } else if !session.queued.isEmpty {
                Pill(text: "\(session.queued.count) queued", icon: nil, tint: nil)
            }
        }
        .fixedSize()
    }
}

/// A small capsule label: tinted, or grey without a tint.
struct Pill: View {
    let text: String
    let icon: String?
    let tint: Color?

    var body: some View {
        HStack(spacing: 3) {
            if let icon { Image(systemName: icon).imageScale(.small) }
            Text(text)
        }
        .font(.app(.caption).weight(.semibold))
        .foregroundStyle(tint.map { AnyShapeStyle($0) } ?? AnyShapeStyle(.secondary))
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .background(tint.map { $0.opacity(0.14) } ?? Theme.chip, in: .capsule)
    }
}

extension View {
    /// Swipe actions (archive, delete) and a context menu (rename too) on a
    /// conversation's row. `main` can't be deleted.
    func conversationActions(
        _ session: AgentSession, rename: @escaping () -> Void, archive: @escaping () -> Void,
        delete: @escaping () -> Void
    ) -> some View {
        let deletable = session.id != AgentSession.main
        return
            self
            .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                // Archiving can be undone, so a full swipe does it.
                Button(session.archived ? "Unarchive" : "Archive", systemImage: "archivebox", action: archive)
                    .tint(.indigo)
                if deletable {
                    Button("Delete", systemImage: "trash", action: delete).tint(.red)
                }
            }
            .contextMenu {
                Button("Rename…", systemImage: "pencil", action: rename)
                Button(
                    session.archived ? "Unarchive" : "Archive",
                    systemImage: session.archived ? "tray.and.arrow.up" : "archivebox", action: archive)
                if deletable {
                    Divider()
                    Button("Delete…", systemImage: "trash", role: .destructive, action: delete)
                }
            }
    }
}

/// Renaming (an alert with the title to edit) and deleting (asked first),
/// for a list of conversations or the chat's title menu.
struct ConversationEditing: ViewModifier {
    let store: ChatStore
    let client: AgentClient
    @Binding var renaming: AgentSession?
    @Binding var deleting: AgentSession?
    @State private var title = ""

    func body(content: Content) -> some View {
        content
            .alert(
                "Rename conversation",
                isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } }),
                presenting: renaming
            ) { session in
                TextField("Title", text: $title)
                Button("Cancel", role: .cancel) {}
                Button("Rename") {
                    let title = title
                    Task { await store.rename(session.id, to: title, client: client) }
                }
            }
            .onChange(of: renaming) { _, session in title = session?.title ?? "" }
            .confirmationDialog(
                "Delete “\(deleting?.displayTitle ?? "")”?",
                isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }),
                titleVisibility: .visible, presenting: deleting
            ) { session in
                Button("Delete", role: .destructive) {
                    Task { await store.delete(session.id, client: client) }
                }
                Button("Cancel", role: .cancel) {}
            } message: { _ in
                Text("Its messages and queued messages are deleted. What it cost stays in Usage.")
            }
    }
}

extension View {
    func conversationEditing(
        store: ChatStore, client: AgentClient, renaming: Binding<AgentSession?>, deleting: Binding<AgentSession?>
    ) -> some View {
        modifier(ConversationEditing(store: store, client: client, renaming: renaming, deleting: deleting))
    }
}

/// Opens a conversation (a new one, or one a banner points to): the home
/// that shows conversations sets it.
struct OpenConversation: Sendable, Equatable {
    let open: @MainActor @Sendable (String) -> Void
    let new: @MainActor @Sendable () -> Void

    static func == (lhs: OpenConversation, rhs: OpenConversation) -> Bool { true }
}

extension EnvironmentValues {
    @Entry var openConversation: OpenConversation? = nil
}

/// The conversations put away with Archive (the iPhone's screen; the Mac
/// shows them in the sidebar).
struct ArchivedConversations: View {
    let store: ChatStore
    let client: AgentClient
    @State private var renaming: AgentSession?
    @State private var deleting: AgentSession?

    var body: some View {
        List {
            if let archived = store.archived, !archived.isEmpty {
                Section {
                    ForEach(archived) { session in
                        NavigationLink(value: ConversationRoute.chat(session.id)) {
                            ConversationRow(session: session)
                        }
                        .conversationActions(
                            session, rename: { renaming = session },
                            archive: { Task { await store.setArchived(false, session.id, client: client) } },
                            delete: { deleting = session })
                    }
                } footer: {
                    Text("Swipe left to bring one back or delete it.").sectionFooter()
                }
            }
        }
        .overlay {
            if store.archived == nil {
                ProgressView()
            } else if store.archived?.isEmpty == true {
                ContentUnavailableView(
                    "Nothing archived", systemImage: "archivebox",
                    description: Text("Swipe left on a conversation to archive it. It comes back here."))
            }
        }
        .navigationTitle("Archived")
        .inlineTitle()
        .conversationEditing(store: store, client: client, renaming: $renaming, deleting: $deleting)
        .task { await store.loadArchived(client: client) }
        .refreshable { await store.loadArchived(client: client) }
    }
}

/// Where the iPhone's list leads.
enum ConversationRoute: Hashable {
    case chat(String)
    case archived
}
