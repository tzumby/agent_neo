import AgentAPI
import Foundation
import Observation

/// One conversation's chat: its timeline, loaded from its newest page (older
/// pages as the user scrolls up), then kept current by the store's one event
/// stream, which routes each event here by its `session`.
@MainActor
@Observable
public final class Conversation {
    public let id: String
    public private(set) var timeline = ChatTimeline()
    /// Its newest page is in: the chat can show, at its end.
    public private(set) var loaded = false
    /// Older chat is on the desktop, not loaded yet.
    public private(set) var hasOlder = false
    /// What the user is typing in it, kept while they look at another one.
    public var draft = ""
    /// Its queued messages as agentd last listed them (GET /sessions), or
    /// answered a send with, until their events come.
    var listed: [QueuedMessage] = []
    /// Queued messages the user removed, hidden before agentd confirms.
    var removing: Set<String> = []

    /// Its events loaded so far, oldest first (the timeline is built from them).
    private(set) var history: [AgentEvent] = []
    /// The newest seq its chat covers: stream events up to it are in already.
    @ObservationIgnored private(set) var through = 0
    /// Stream events that came while its newest page was on its way.
    @ObservationIgnored private var buffered: [AgentEvent]?
    @ObservationIgnored var newestPage: Task<Void, any Error>?
    @ObservationIgnored var olderPage: Task<Bool, Never>?
    /// Cards the desktop no longer knows (answered 404), kept across rebuilds.
    @ObservationIgnored private var expiredApprovals: Set<String> = []
    @ObservationIgnored private var expiredActions: Set<String> = []

    init(id: String) {
        self.id = id
    }

    public var busy: Bool { timeline.busy }
    public var stopping: Bool { timeline.stopping }
    /// The seq of the oldest event loaded: changes with each older page.
    public var oldestLoaded: Int? { history.first?.seq }

    /// Its queued messages, in the order they'll start.
    public var queue: [QueuedMessage] {
        timeline.queue(merging: listed).filter { !removing.contains($0.id) }
    }

    /// Its newest page is on its way: hold the stream's events until it's in.
    func loading() {
        if buffered == nil { buffered = [] }
    }

    /// The newest page didn't come.
    func failedLoading() {
        buffered = nil
    }

    /// The newest page of the chat, in place of what was loaded, then what
    /// the stream sent meanwhile.
    func show(newest page: EventPage) {
        history = page.events
        hasOlder = page.more
        through = page.lastSeq
        rebuild()
        loaded = true
        flush()
    }

    /// agentd before 0.15.1 has no pages: the stream replays the whole log.
    func showStream() {
        loaded = true
        flush()
    }

    private func flush() {
        let waiting = buffered ?? []
        buffered = nil
        waiting.forEach(receive)
    }

    /// An event of this conversation (or a global one) from the live stream.
    func receive(_ event: AgentEvent) {
        if buffered != nil {
            buffered?.append(event)
            return
        }
        guard loaded, event.seq > through else { return }
        through = event.seq
        history.append(event)
        timeline.apply(event)
    }

    /// An older page, in front of what's loaded.
    func insert(older page: EventPage) {
        history.insert(contentsOf: page.events, at: 0)
        hasOlder = page.more
        rebuild()
    }

    /// Hides a queued message the user removed, or brings it back.
    func hide(queued id: String, _ hidden: Bool = true) {
        if hidden { removing.insert(id) } else { removing.remove(id) }
    }

    /// A send was queued: show its bubble before its event comes.
    func list(queued message: QueuedMessage) {
        listed.append(message)
    }

    func set(listed queue: [QueuedMessage]) {
        if listed != queue { listed = queue }
    }

    func markStopping() {
        timeline.markStopping()
    }

    func expire(approval id: String) {
        expiredApprovals.insert(id)
        timeline.expire(approval: id)
    }

    func expire(action id: String) {
        expiredActions.insert(id)
        timeline.expire(action: id)
    }

    /// The timeline again from all the events loaded (an answer in a newer
    /// page closes a card from an older one).
    private func rebuild() {
        var rebuilt = ChatTimeline(replaying: history)
        expiredApprovals.forEach { rebuilt.expire(approval: $0) }
        expiredActions.forEach { rebuilt.expire(action: $0) }
        if timeline.stopping { rebuilt.markStopping() }
        timeline = rebuilt
    }
}
