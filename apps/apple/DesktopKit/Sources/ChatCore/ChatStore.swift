import AgentAPI
import Foundation
import Observation

/// Keeps one desktop's conversations in sync: the list of them, and each
/// one's chat (`Conversation`), opened from its newest page with older pages
/// as the user scrolls up. One event stream serves them all, routed by each
/// event's `session` (reconnecting and resuming after the last one seen).
/// Sends the user's commands. A desktop from before conversations (agentd
/// before 0.16.0) has one, `main`.
@MainActor
@Observable
public final class ChatStore {
    public enum Connection: Equatable, Sendable {
        case connecting
        case live
        case offline(String)
    }

    public internal(set) var connection: Connection = .connecting
    public private(set) var status: AgentStatus?
    /// A conversation's newest page is in: the chat can show, at its end.
    public private(set) var ready = false
    /// A one-off problem to show the user (busy, network, …).
    public var notice: String?

    /// The desktop has conversations (agentd 0.16.0). false: an older one,
    /// which is only `main`. nil until it answers.
    public private(set) var conversationsAvailable: Bool?
    /// The conversations, most recently updated first (not the archived ones).
    public private(set) var sessions: [AgentSession] = []
    /// The archived conversations, once asked for (`loadArchived`).
    public private(set) var archived: [AgentSession]?
    /// Conversations open but in neither list: archived ones, before the
    /// archived list is asked for.
    private var unlisted: [String: AgentSession] = [:]
    /// How many conversations can work at the same moment.
    public private(set) var runningLimit = 3
    /// The conversation on screen, remembered on this device.
    public private(set) var selected: String

    /// Each conversation's chat, made the first time it's asked for. A
    /// cache, so not observed (each one is).
    @ObservationIgnored private var conversations: [String: Conversation] = [:]
    /// The newest event applied: the stream goes on after it.
    private(set) var lastSeq = 0
    /// Where the conversation on screen is remembered (UserDefaults).
    @ObservationIgnored private let rememberedKey: String?
    /// Debounces fetching the list again.
    @ObservationIgnored private var listGeneration = 0
    @ObservationIgnored private var archivedChanged = false

    /// Events per page: a dozen or so exchanges.
    static let pageSize = 60

    /// `remembering`: the UserDefaults key for the conversation last on screen.
    public init(remembering key: String? = nil) {
        rememberedKey = key
        selected = key.flatMap { UserDefaults.standard.string(forKey: $0) } ?? AgentSession.main
    }

    /// Events after which the status (its stage above all) is worth fetching again.
    static let changesStatus: Set<String> = [
        "settings", "task.started", "task.paused", "task.resumed", "task.finished", "approval.requested",
        "approval.resolved", "action_needed", "action.resolved", "secret.saved",
        "memory.confirmed", "memory.forgotten", "schedule.created", "schedule.updated", "schedule.deleted",
        "budget.alert",
    ]

    /// Conversations created, renamed, archived or deleted; the budget alerts.
    static let listKinds: Set<String> = ["session.created", "session.updated", "session.deleted", "budget.alert"]

    /// What the live stream asks for: the chat, what changes the status, and
    /// what changes the list of conversations.
    static let streamed = ChatTimeline.kinds.union(changesStatus).union(listKinds)

    /// Events after which the list is worth fetching again: its states,
    /// queues and last messages change with them (rather than being worked
    /// out here).
    nonisolated static func changesList(_ kind: String) -> Bool {
        kind.hasPrefix("session.") || kind.hasPrefix("message") || kind.hasPrefix("task.")
            || kind.hasPrefix("approval.") || kind.hasPrefix("action") || kind == "budget.alert"
    }

    /// The kinds that are a conversation's even without a `session` (with any
    /// event of a task): docs/conversations-and-usage.md §3.3.
    nonisolated static let conversationKinds: Set<String> = [
        "message", "step", "plan", "review", "shell", "shell.output", "action_needed", "action.resolved", "image",
        "message.queued", "message.unqueued",
    ]

    /// The conversation an event belongs to: its `session`. An older agentd
    /// sends none; then a conversation's event (a message, a task's, a
    /// card's) is `main`'s. nil: a global event (a setting, a saved key),
    /// which every conversation applies.
    public nonisolated static func conversation(of event: AgentEvent) -> String? {
        if let session = event.data["session"]?.string, !session.isEmpty { return session }
        let kind = event.kind
        if conversationKinds.contains(kind) || kind.hasPrefix("task.") || kind.hasPrefix("run.")
            || kind.hasPrefix("approval.") || event.data["task"]?.string != nil
        {
            return AgentSession.main
        }
        return nil
    }

    // MARK: Conversations

    /// A conversation's chat.
    public func conversation(_ id: String) -> Conversation {
        if let chat = conversations[id] { return chat }
        let chat = Conversation(id: id)
        chat.set(listed: sessions.first { $0.id == id }?.queued ?? [])
        conversations[id] = chat
        return chat
    }

    /// The conversation on screen.
    public var current: Conversation { conversation(selected) }

    // The conversation on screen's chat, as before conversations.
    public var timeline: ChatTimeline { current.timeline }
    public var busy: Bool { current.busy }
    public var stopping: Bool { current.stopping }
    public var loaded: Bool { current.loaded }
    public var hasOlder: Bool { current.hasOlder }
    public var oldestLoaded: Int? { current.oldestLoaded }

    /// What agentd last said about a conversation (nil: not listed, or not yet).
    public func session(_ id: String) -> AgentSession? {
        sessions.first { $0.id == id } ?? archived?.first { $0.id == id } ?? unlisted[id]
    }

    /// Shows a conversation, and remembers it for the next launch.
    public func select(_ id: String) {
        guard conversationsAvailable != false, id != selected else { return }
        selected = id
        if let rememberedKey { UserDefaults.standard.set(id, forKey: rememberedKey) }
    }

    /// An Action needed card, in whichever conversation has it (for the take-over view).
    public func action(_ id: String?) -> Action? {
        guard let id else { return nil }
        for chat in conversations.values {
            for case .action(let action) in chat.timeline.items where action.id == id { return action }
        }
        return nil
    }

    /// The conversation (of those loaded) where a task started.
    public func conversation(startingTask task: String) -> String? {
        conversations.values.first { $0.timeline.itemID(startingTask: task) != nil }?.id
    }

    // MARK: Syncing

    /// Runs until the calling task is cancelled (e.g. the view disappears or
    /// the Tailscale session is replaced).
    public func run(client: AgentClient) async {
        while !Task.isCancelled {
            connection = .connecting
            do {
                status = try await client.status()
                // The desktop answered; a quiet chat may not send an event for a while.
                connection = .live
                // First, so pages are asked for with or without a conversation.
                try await refreshList(client: client)
                try await load(current, client: client)
                for try await event in client.events(after: streamStart, kinds: Self.streamed) {
                    guard event.seq > lastSeq else { continue }
                    receive(event)
                    if Self.changesList(event.kind) { listChanged(event.kind, client: client) }
                    if Self.changesStatus.contains(event.kind) {
                        status = try? await client.status()
                    }
                }
            } catch {
                if Task.isCancelled { break }
                connection = .offline(error.localizedDescription)
            }
            try? await Task.sleep(for: .seconds(2))
        }
    }

    /// Where the stream starts: after the last event applied or, before any
    /// came, after the oldest of the newest pages loaded (each conversation
    /// skips what its own page had).
    private var streamStart: Int {
        if lastSeq > 0 { return lastSeq }
        return conversations.values.filter(\.loaded).map(\.through).min() ?? 0
    }

    /// Opens a conversation: loads its newest page, again while the desktop
    /// doesn't answer, as long as the caller waits.
    public func open(_ id: String, client: AgentClient) async {
        let chat = conversation(id)
        while !chat.loaded, !Task.isCancelled {
            // The stream's first round decides whether pages are per conversation.
            if connection == .live, conversationsAvailable != nil {
                do {
                    try await load(chat, client: client)
                    return
                } catch {}
            }
            try? await Task.sleep(for: .seconds(1))
        }
    }

    /// A conversation's newest page, in one go: its chat appears at its end
    /// rather than filling in event by event. A load under way is joined.
    func load(_ chat: Conversation, client: AgentClient) async throws {
        if chat.loaded { return }
        if let pending = chat.newestPage { return try await pending.value }
        chat.loading()
        let session = pageSession(chat.id)
        let load = Task {
            defer { chat.newestPage = nil }
            do {
                let page = try await client.eventPage(
                    before: nil, limit: Self.pageSize, kinds: ChatTimeline.kinds, session: session)
                chat.show(newest: scoped(page, to: session))
            } catch let error as AgentError where error.statusCode == 404 {
                // agentd before 0.15.1 has no pages: the stream replays the whole log.
                chat.showStream()
            } catch {
                chat.failedLoading()
                throw error
            }
            ready = true
        }
        chat.newestPage = load
        try await load.value
    }

    /// Pages are per conversation once the desktop has them.
    private func pageSession(_ id: String) -> String? {
        conversationsAvailable == true ? id : nil
    }

    /// Only the conversation's own events and the global ones, should a page
    /// bring others.
    private func scoped(_ page: EventPage, to session: String?) -> EventPage {
        guard let session else { return page }
        let events = page.events.filter { event in Self.conversation(of: event).map { $0 == session } ?? true }
        return events.count == page.events.count ? page : EventPage(events: events, more: page.more, lastSeq: page.lastSeq)
    }

    /// The newest page of the conversation on screen, in place of what was loaded.
    func show(newest page: EventPage) {
        current.show(newest: page)
        lastSeq = page.lastSeq
        ready = true
    }

    /// An event from the live stream, to its conversation (or, a global one, to all).
    func receive(_ event: AgentEvent) {
        lastSeq = max(lastSeq, event.seq)
        guard ChatTimeline.kinds.contains(event.kind) else { return }
        if let id = Self.conversation(of: event) {
            conversations[id]?.receive(event)
        } else {
            conversations.values.forEach { $0.receive(event) }
        }
    }

    /// An older page of the conversation on screen, in front of what's loaded.
    func insert(older page: EventPage) {
        current.insert(older: page)
    }

    /// The page before the oldest one loaded of the conversation on screen.
    @discardableResult
    public func loadOlder(client: AgentClient) async -> Bool {
        await loadOlder(current, client: client)
    }

    /// The page before the oldest one loaded, put in front of it. Returns
    /// whether one came. A load already under way is joined, not repeated.
    @discardableResult
    public func loadOlder(_ chat: Conversation, client: AgentClient) async -> Bool {
        if let olderPage = chat.olderPage { return await olderPage.value }
        guard chat.hasOlder, let before = chat.oldestLoaded else { return false }
        let session = pageSession(chat.id)
        // Not tied to the caller: a page asked for by a row that scrolls away still lands.
        let load = Task {
            defer { chat.olderPage = nil }
            guard
                let page = try? await client.eventPage(
                    before: before, limit: Self.pageSize, kinds: ChatTimeline.kinds, session: session)
            else { return false }
            chat.insert(older: scoped(page, to: session))
            return true
        }
        chat.olderPage = load
        return await load.value
    }

    // MARK: The list

    /// The list of conversations, again. An older agentd (404) has only `main`.
    public func refreshList(client: AgentClient) async throws {
        let list: SessionList
        do {
            list = try await client.sessions()
        } catch let error as AgentError where error.statusCode == 404 {
            conversationsAvailable = false
            if !sessions.isEmpty { sessions = [] }
            // Not remembered: the desktop may be updated later.
            selected = AgentSession.main
            return
        }
        apply(list)
        await checkSelected(client: client)
    }

    /// The list as agentd sent it, and each open conversation's queue from it.
    func apply(_ list: SessionList) {
        if sessions != list.sessions { sessions = list.sessions }
        if runningLimit != list.runningLimit { runningLimit = list.runningLimit }
        for chat in conversations.values {
            chat.set(listed: list.sessions.first { $0.id == chat.id }?.queued ?? [])
        }
        if conversationsAvailable != true { conversationsAvailable = true }
    }

    /// The conversation remembered may be gone (deleted on another device):
    /// then `main`. One that's archived isn't listed but stays.
    private func checkSelected(client: AgentClient) async {
        let id = selected
        guard session(id) == nil else { return }
        do {
            unlisted[id] = try await client.session(id)
        } catch let error as AgentError where error.statusCode == 404 {
            if selected == id { select(AgentSession.main) }
        } catch {}
    }

    /// Fetches the list again ~300 ms after the last event that changes it.
    private func listChanged(_ kind: String, client: AgentClient) {
        guard conversationsAvailable == true else { return }
        if kind.hasPrefix("session.") { archivedChanged = true }
        listGeneration += 1
        let generation = listGeneration
        Task {
            try? await Task.sleep(for: .milliseconds(300))
            guard generation == listGeneration else { return }
            try? await refreshList(client: client)
            if archived != nil, archivedChanged {
                archivedChanged = false
                await loadArchived(client: client)
            }
        }
    }

    /// The archived conversations (for the list's Archived section).
    public func loadArchived(client: AgentClient) async {
        do {
            let list = try await client.sessions(archived: true).sessions
            if archived != list { archived = list }
        } catch {
            if archived == nil { archived = [] }
        }
    }

    /// Selects a new conversation: an empty one if there is one, rather than
    /// adding another. Returns its id (nil: it couldn't be made).
    @discardableResult
    public func newConversation(client: AgentClient) async -> String? {
        if let blank = sessions.first(where: \.isBlank) {
            select(blank.id)
            return blank.id
        }
        do {
            let made = try await client.createSession()
            if !sessions.contains(where: { $0.id == made.id }) { sessions.insert(made, at: 0) }
            select(made.id)
            return made.id
        } catch {
            notice = "Couldn't start a new conversation. \(Self.message(error))"
            return nil
        }
    }

    public func rename(_ id: String, to title: String, client: AgentClient) async {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !title.isEmpty, session(id)?.title != title else { return }
        change(id) { $0.title = title }  // at once; agentd's answer replaces it
        do {
            replace(try await client.updateSession(id, title: title))
        } catch {
            notice = "Couldn't rename it. \(Self.message(error))"
            try? await refreshList(client: client)
        }
    }

    /// Archives (hides from the list) or brings back a conversation.
    public func setArchived(_ archive: Bool, _ id: String, client: AgentClient) async {
        let title = session(id)?.displayTitle ?? "This conversation"
        do {
            let updated = try await client.updateSession(id, archived: archive)
            sessions.removeAll { $0.id == id }
            archived?.removeAll { $0.id == id }
            unlisted[id] = nil
            if !archive {
                sessions.insert(updated, at: 0)
            } else if archived != nil {
                archived?.insert(updated, at: 0)
            } else {
                unlisted[id] = updated
            }
            try? await refreshList(client: client)
        } catch let error as AgentError where error.statusCode == 409 {
            notice = "“\(title)” is working or waiting for you. Stop it, or answer its card, first."
        } catch {
            notice = "Couldn't \(archive ? "archive" : "unarchive") it. \(Self.message(error))"
        }
    }

    /// Deletes a conversation's history and queued messages (not `main`).
    public func delete(_ id: String, client: AgentClient) async {
        guard id != AgentSession.main else { return }
        let title = session(id)?.displayTitle ?? "This conversation"
        do {
            try await client.deleteSession(id)
            forget(id)
        } catch let error as AgentError where error.statusCode == 404 {
            forget(id)  // gone already
        } catch let error as AgentError where error.statusCode == 409 {
            notice = "“\(title)” is working or waiting for you. Stop it, or answer its card, first."
        } catch {
            notice = "Couldn't delete it. \(Self.message(error))"
        }
    }

    private func forget(_ id: String) {
        sessions.removeAll { $0.id == id }
        archived?.removeAll { $0.id == id }
        unlisted[id] = nil
        conversations[id] = nil
        if selected == id { select(sessions.first?.id ?? AgentSession.main) }
    }

    private func change(_ id: String, _ edit: (inout AgentSession) -> Void) {
        if let index = sessions.firstIndex(where: { $0.id == id }) { edit(&sessions[index]) }
        if let index = archived?.firstIndex(where: { $0.id == id }) { edit(&archived![index]) }
        if var session = unlisted[id] {
            edit(&session)
            unlisted[id] = session
        }
    }

    private func replace(_ session: AgentSession) {
        change(session.id) { $0 = session }
    }

    // MARK: Finding a row

    /// Where the chat row a notification points to (`approval-<id>`,
    /// `action-<id>`) stands.
    public enum Lookup: Equatable, Sendable {
        /// Waiting for the user: scroll to it.
        case waiting
        /// In the chat, but answered or expired.
        case settled
        /// Nowhere in the chat's history.
        case missing
        /// The desktop didn't answer in time, so the chat may not be current.
        case unreachable
    }

    /// Finds the row a notification points to, in a conversation (the one on
    /// screen by default). After the app was suspended the chat is behind,
    /// so it first waits until it's current: the row arrives on the stream,
    /// or the stream has applied every chat event the desktop had when
    /// asked. Only then does it page back, since the row is older than
    /// what's loaded. `connecting` bounds the wait for the connection
    /// (Tailscale starts after a cold launch), `catchingUp` the wait after it.
    public func locate(
        _ itemID: String, in id: String? = nil, client: AgentClient,
        connecting: Duration = .seconds(45), catchingUp: Duration = .seconds(15)
    ) async -> Lookup {
        let chat = conversation(id ?? selected)
        return await locate(
            itemID, in: chat, connecting: connecting, catchingUp: catchingUp, poll: .milliseconds(250),
            newestChatSeq: {
                try await client.eventPage(
                    before: nil, limit: 1, kinds: ChatTimeline.kinds, session: self.pageSession(chat.id)
                ).events.last?.seq ?? 0
            },
            older: { await self.loadOlder(chat, client: client) })
    }

    func locate(
        _ itemID: String, in chat: Conversation? = nil, connecting: Duration, catchingUp: Duration, poll: Duration,
        newestChatSeq: () async throws -> Int, older: () async -> Bool
    ) async -> Lookup {
        let chat = chat ?? current
        let clock = ContinuousClock()
        var deadline = clock.now.advanced(by: connecting)
        var connected = false
        var newest: Int?
        while !holds(itemID, chat) {
            if Task.isCancelled { return .unreachable }
            if connection == .live, chat.loaded {
                if !connected {
                    connected = true
                    deadline = clock.now.advanced(by: catchingUp)
                }
                if newest == nil { newest = try? await newestChatSeq() }
                if let newest, max(lastSeq, chat.through) >= newest { break }
            }
            if clock.now >= deadline { return .unreachable }
            try? await Task.sleep(for: poll)
        }
        // The chat is current and the row isn't in it: it's older than what's loaded.
        while !holds(itemID, chat) {
            if Task.isCancelled { return .unreachable }
            guard await older() else { return chat.hasOlder ? .unreachable : .missing }
        }
        return chat.timeline.visibleItems.contains { $0.id == itemID } ? .waiting : .settled
    }

    private func holds(_ itemID: String, _ chat: Conversation) -> Bool {
        chat.timeline.items.contains { $0.id == itemID }
    }

    // MARK: Commands

    /// Sends a message to the conversation on screen.
    public func send(_ text: String, client: AgentClient) async {
        await send(text, to: selected, client: client)
    }

    /// Sends a message: it starts a task, or (agentd 0.16.0) waits its turn
    /// behind the one running, shown as a queued bubble at once.
    public func send(_ text: String, to id: String, client: AgentClient) async {
        let chat = conversation(id)
        do {
            let sent =
                conversationsAvailable == true ? try await client.send(text, session: id) : try await client.send(text)
            if case .queued(let queued, _) = sent {
                chat.list(queued: QueuedMessage(id: queued, text: text))
            }
        } catch let error as AgentError where error.statusCode == 404 && conversationsAvailable == true {
            notice = "This conversation was archived or deleted on another device."
        } catch {
            handle(error)
        }
    }

    /// Takes a queued message out of the queue (its ✕). Gone already
    /// (started, or removed elsewhere) is fine.
    public func remove(_ message: QueuedMessage, from id: String, client: AgentClient) async {
        let chat = conversation(id)
        chat.hide(queued: message.id)
        do {
            try await client.unqueue(message.id, session: id)
        } catch let error as AgentError where error.statusCode == 404 {
        } catch {
            chat.hide(queued: message.id, false)
            notice = "Couldn't remove it. \(Self.message(error))"
        }
    }

    /// Stops a conversation's task, the one on screen by default (or gives up
    /// on its card), and drops its queued messages.
    public func stop(_ id: String? = nil, client: AgentClient) async {
        let chat = conversation(id ?? selected)
        // Show it at once; agentd confirms with task.stopping, then task.finished.
        chat.markStopping()
        do {
            if conversationsAvailable == true {
                try await client.stop(session: chat.id)
            } else {
                try await client.stop()
            }
            // Dropped: their message.unqueued events follow.
            chat.queue.forEach { chat.hide(queued: $0.id) }
        } catch let error as AgentError where error.statusCode == 404 && conversationsAvailable == true {
            notice = "This conversation was deleted on another device."
        } catch {
            handle(error)
        }
    }

    public func answer(_ approval: Approval, approved: Bool, text: String? = nil, client: AgentClient) async {
        await attempt(onGone: { $0.conversations.values.forEach { $0.expire(approval: approval.id) } }) {
            try await client.answer(approval: approval.id, approved: approved, text: text)
        }
    }

    public func resolve(_ action: Action, done: Bool, client: AgentClient) async {
        await attempt(onGone: { $0.conversations.values.forEach { $0.expire(action: action.id) } }) {
            try await client.resolve(action: action.id, done: done)
        }
    }

    /// Saves the key a request card asked for; the waiting task carries on.
    /// Returns whether it was saved.
    @discardableResult
    public func save(secret value: String, for approval: Approval, client: AgentClient) async -> Bool {
        guard let request = approval.secret else { return false }
        do {
            let saved = try await client.saveSecret(
                name: request.name, value: value, label: request.label, approval: approval.id)
            if saved.busy {
                notice = "Saved. Your desktop is busy with another request; tap Continue when it's done."
            }
            return true
        } catch let error as AgentError where error.statusCode == 400 {
            if case .http(_, let message) = error { notice = "Couldn't save it: \(message)" }
            return false
        } catch {
            notice = "Couldn't save it: \(error.localizedDescription)"
            return false
        }
    }

    /// Changes the model or browser actions. Returns what went wrong, for the
    /// screen that asked to show (nil: saved). The status is read again either
    /// way, so controls show what the desktop has.
    @discardableResult
    public func updateSettings(model: String? = nil, allowActions: Bool? = nil, client: AgentClient) async -> String? {
        var problem: String?
        do {
            _ = try await client.updateSettings(model: model, allowActions: allowActions)
        } catch {
            problem = "Couldn't save the setting. \(Self.message(error))"
        }
        await refreshStatus(client: client)
        return problem
    }

    /// Turns Auto approve on (for the revision the user reviewed) or off.
    /// Returns what went wrong, as `updateSettings` does.
    @discardableResult
    public func setAutoApprove(_ enabled: Bool, expectedVersion: String?, client: AgentClient) async -> String? {
        var problem: String?
        do {
            _ = try await client.setAutoApprove(enabled, expectedVersion: expectedVersion)
        } catch {
            problem = "Couldn't turn Auto approve \(enabled ? "on" : "off"). \(Self.message(error))"
        }
        await refreshStatus(client: client)
        return problem
    }

    /// Sets the monthly budget, or removes it (nil). Returns what went wrong,
    /// as `updateSettings` does.
    @discardableResult
    public func setMonthlyBudget(_ usd: Double?, client: AgentClient) async -> String? {
        var problem: String?
        do {
            try await client.setMonthlyBudget(usd)
        } catch {
            problem = "Couldn't save the budget. \(Self.message(error))"
        }
        await refreshStatus(client: client)
        return problem
    }

    /// Keeps the last status when the desktop doesn't answer.
    private func refreshStatus(client: AgentClient) async {
        if let fresh = try? await client.status() { status = fresh }
    }

    /// agentd's own words for an error it answered with.
    public static func message(_ error: Error) -> String {
        if case .http(_, let message)? = error as? AgentError, !message.isEmpty { return message }
        return error.localizedDescription
    }

    private func attempt(onGone: ((ChatStore) -> Void)? = nil, _ work: () async throws -> Void) async {
        do {
            try await work()
        } catch let error as AgentError where error.statusCode == 404 && onGone != nil {
            onGone?(self)
            notice = "That request has expired."
        } catch {
            handle(error)
        }
    }

    private func handle(_ error: Error) {
        guard let error = error as? AgentError, error.statusCode == 409 else {
            notice = error.localizedDescription
            return
        }
        if conversationsAvailable == true {
            // agentd 0.16.0 queues; only answering a card while another of
            // the same conversation resumes is refused, for a moment.
            notice = "Your desktop is busy with another request in this conversation. Try again in a moment."
        } else {
            // An older desktop runs one task at a time.
            notice = "Your desktop is busy with another request. Stop it first."
        }
    }
}
