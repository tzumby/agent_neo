import AgentAPI
import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// What the agent remembers between conversations (docs/memory-v2.md §14).
/// With memory v2: what waits for approval first, grouped by why; then the
/// memories by kind, searchable, with expired ones restorable; an edit sheet
/// with "Why do you know this?"; consolidation's mode and cost; export and
/// import. A desktop without memory v2 gets v1's confirm-or-forget list.
struct MemoryView: View {
    let client: AgentClient
    @State private var memories: [AgentMemory] = []
    @State private var stats: ConsolidationStats?
    @State private var kind = "rule"
    @State private var showGone = false
    @State private var query = ""
    @State private var loaded = false
    @State private var problem: String?
    @State private var notice: String?
    @State private var editing: AgentMemory?
    @State private var importing = false
    @State private var sharing: ShareFile?

    /// The desktop runs memory v2 (its consolidation answers).
    private var v2: Bool { stats != nil || memories.contains { $0.kind != nil } }

    var body: some View {
        List {
            if v2 { v2Sections } else { v1Sections }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle("Memory")
        .navigationBarTitleDisplayMode(.inline)
        .searchable(text: $query, prompt: "Search memories")
        .toolbar {
            if v2 {
                Menu("More", systemImage: "ellipsis.circle") {
                    Toggle("Show expired and replaced", isOn: $showGone)
                    Section("Export") {
                        Button("As JSON (to import elsewhere)") { Task { await export(markdown: false) } }
                        Button("As Markdown (to read)") { Task { await export(markdown: true) } }
                    }
                    Button("Import…", systemImage: "square.and.arrow.down") { importing = true }
                }
            }
        }
        .overlay { if !loaded { ProgressView() } }
        .task(id: "\(query)|\(showGone)") { await load() }
        .refreshable { await load() }
        .sheet(item: $editing) { memory in
            NavigationStack { MemoryEditor(client: client, memory: memory) { await load() } }
        }
        .sheet(item: $sharing) { file in ShareSheet(url: file.url) }
        .fileImporter(isPresented: $importing, allowedContentTypes: [.json]) { result in
            Task { await importFile(result) }
        }
        .alert("Memory", isPresented: Binding(get: { notice != nil }, set: { if !$0 { notice = nil } })) {
            Button("OK") {}
        } message: {
            Text(notice ?? "")
        }
    }

    // MARK: Memory v2

    @ViewBuilder private var v2Sections: some View {
        let pending = memories.filter(\.isPending)
        if query.isEmpty {
            ForEach(Self.reasons, id: \.self) { reason in
                let group = pending.filter { ($0.pendingReason ?? "other") == reason }
                if !group.isEmpty {
                    Section {
                        ForEach(group) { memory in
                            PendingRow(memory: memory) { approve in Task { await answer(memory, approve: approve) } }
                        }
                    } header: {
                        Text(Self.groupTitle(reason))
                    } footer: {
                        VStack(alignment: .leading, spacing: 8) {
                            Text(Self.groupFooter(reason))
                            if group.count > 1 {
                                HStack(spacing: 16) {
                                    Button("Approve all") { Task { await answerAll(reason, approve: true) } }
                                    Button("Reject all", role: .destructive) {
                                        Task { await answerAll(reason, approve: false) }
                                    }
                                }
                                .font(.footnote.weight(.semibold))
                                .buttonStyle(.borderless)
                            }
                        }
                    }
                }
            }
        }
        Section {
            if query.isEmpty {
                Picker("Kind", selection: $kind) {
                    Text("Rules").tag("rule")
                    Text("Preferences").tag("preference")
                    Text("Facts").tag("fact")
                    Text("Work log").tag("work_log")
                }
                .pickerStyle(.segmented)
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets())
            }
            let shown = memories.filter { memory in
                !memory.isPending && (query.isEmpty ? memory.kind == kind : true)
                    && (showGone ? true : !memory.isGone)
            }
            if loaded, shown.isEmpty {
                Text(query.isEmpty ? Self.empty(kind) : "Nothing matches.").foregroundStyle(.secondary)
            }
            ForEach(shown) { memory in
                Button { editing = memory } label: { MemoryRow(memory: memory, showKind: !query.isEmpty) }
                    .foregroundStyle(.primary)
                    .swipeActions(edge: .trailing) {
                        if !memory.isGone {
                            Button("Forget", role: .destructive) { Task { await forget(memory) } }
                        }
                    }
                    .swipeActions(edge: .leading) {
                        if memory.isGone {
                            Button("Restore") { Task { await restore(memory) } }.tint(.blue)
                        } else if memory.isInferred {
                            Button("Confirm") { Task { await answer(memory, approve: true) } }.tint(.green)
                        }
                    }
            }
        } footer: {
            if query.isEmpty {
                Text("Tap one to edit it. Swipe left to forget it; right to confirm a hint, or to restore an expired one.")
            }
        }
        if query.isEmpty, let stats {
            ConsolidationSection(stats: stats, client: client) { await load() }
        }
    }

    private static let reasons = ["tainted_rule", "conflict", "proposed", "imported", "other"]

    private static func groupTitle(_ reason: String) -> String {
        switch reason {
        case "tainted_rule": "From outside content"
        case "conflict": "Corrections to what you said"
        case "proposed": "Proposed while tidying up"
        case "imported": "Imported"
        default: "Waiting for you"
        }
    }

    private static func groupFooter(_ reason: String) -> String {
        switch reason {
        case "tainted_rule":
            "Saved after the agent read a page, an email or command output, so they could come from that content. They aren't used until you approve them."
        case "conflict": "Approving one replaces what you said before."
        case "proposed": "The agent's suggestions from tidying up its memory."
        case "imported": "From an import: nothing counts until you approve it."
        default: "The agent doesn't use these until you approve them."
        }
    }

    private static func empty(_ kind: String) -> String {
        switch kind {
        case "rule": "No rules yet. Tell the agent, like “From now on, reply in English.”"
        case "preference": "No preferences yet, like “I prefer summaries under 100 words.”"
        case "work_log": "No work log yet: it's written after tasks, while the agent tidies up."
        default: "No facts yet. Tell the agent something to keep, like “Remember that my manager is Lawrence.”"
        }
    }

    // MARK: Memory v1

    @ViewBuilder private var v1Sections: some View {
        let unverified = memories.filter { !$0.trusted }
        let trusted = memories.filter(\.trusted)
        if loaded, memories.isEmpty {
            Section {
                Text("Nothing yet. Tell the agent something to keep, like “Remember that my manager is Lawrence.”")
                    .foregroundStyle(.secondary)
            }
        }
        if !unverified.isEmpty {
            Section {
                ForEach(unverified) { memory in
                    Text(memory.text)
                        .swipeActions(edge: .leading) {
                            Button("Confirm") { Task { await answer(memory, approve: true) } }.tint(.green)
                        }
                        .swipeActions(edge: .trailing) {
                            Button("Forget", role: .destructive) { Task { await forget(memory) } }
                        }
                }
            } header: {
                Text("To confirm")
            } footer: {
                Text("Saved after the agent read a web page, an email or command output, so they could come from that content. It treats them as hints until you confirm them: swipe right to confirm, left to forget.")
            }
        }
        if !trusted.isEmpty {
            Section("Remembered") {
                ForEach(trusted) { memory in
                    Text(memory.text)
                        .swipeActions(edge: .trailing) {
                            Button("Forget", role: .destructive) { Task { await forget(memory) } }
                        }
                }
            }
        }
    }

    // MARK: Actions

    private func load() async {
        do {
            stats = try? await client.consolidation()
            if stats != nil {
                let statuses = showGone ? ["active", "pending", "expired", "superseded"] : ["active", "pending"]
                memories = try await client.memories(statuses: statuses, query: query.isEmpty ? nil : query)
            } else {
                memories = try await client.memories()
            }
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
        loaded = true
    }

    private func attempt(_ work: () async throws -> Void) async {
        do {
            try await work()
        } catch {
            problem = error.localizedDescription
        }
        await load()
    }

    private func forget(_ memory: AgentMemory) async {
        memories.removeAll { $0.id == memory.id }
        await attempt { try await client.forget(memory: memory.id) }
    }

    private func answer(_ memory: AgentMemory, approve: Bool) async {
        memories.removeAll { $0.id == memory.id && !approve }
        await attempt {
            if approve { try await client.confirm(memory: memory.id) } else { try await client.reject(memory: memory.id) }
        }
    }

    private func answerAll(_ reason: String, approve: Bool) async {
        let group = reason == "other" ? nil : reason
        await attempt {
            if approve { try await client.approveAll(reason: group) } else { try await client.rejectAll(reason: group) }
        }
    }

    private func restore(_ memory: AgentMemory) async {
        await attempt { try await client.restore(memory: memory.id) }
    }

    private func export(markdown: Bool) async {
        do {
            let data = try await client.exportMemory(markdown: markdown)
            let url = FileManager.default.temporaryDirectory
                .appending(path: markdown ? "Memories.md" : "memories.json")
            try data.write(to: url, options: [.atomic, .completeFileProtection])
            sharing = ShareFile(url: url)
        } catch {
            problem = error.localizedDescription
        }
    }

    private func importFile(_ result: Result<URL, Error>) async {
        do {
            let url = try result.get()
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            let done = try await client.importMemory(try Data(contentsOf: url))
            notice = done.rejected.isEmpty
                ? "Imported \(done.imported). They're waiting for your approval."
                : "Imported \(done.imported); skipped \(done.rejected.count) (\(done.rejected.first?.reason ?? ""))."
        } catch {
            problem = error.localizedDescription
        }
        await load()
    }
}

/// A memory waiting for approval: its text, what it would replace, and the buttons.
private struct PendingRow: View {
    let memory: AgentMemory
    let answer: (Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(MemoryRow.kindName(memory.kind)).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                Spacer()
                Text(String(memory.created.prefix(10))).font(.caption).foregroundStyle(.tertiary)
            }
            Text(memory.text)
            ForEach(memory.replaces ?? [], id: \.id) { old in
                HStack(alignment: .top, spacing: 6) {
                    Text("Replaces").font(.caption.weight(.semibold))
                    Text(old.text).strikethrough()
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            HStack(spacing: 10) {
                Button("Reject", role: .destructive) { answer(false) }
                    .buttonStyle(.bordered)
                Button("Approve") { answer(true) }
                    .buttonStyle(.borderedProminent)
                    .tint(.green)
            }
            .controlSize(.small)
        }
        .padding(.vertical, 2)
    }
}

struct MemoryRow: View {
    let memory: AgentMemory
    var showKind = false

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(memory.text)
                .strikethrough(memory.isGone)
                .foregroundStyle(memory.isGone ? .secondary : .primary)
            Text(detail).font(.caption).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(.rect)
        .padding(.vertical, 1)
    }

    private var detail: String {
        var parts: [String] = []
        if showKind { parts.append(Self.kindName(memory.kind)) }
        parts.append(String(memory.created.prefix(10)))
        switch memory.status {
        case "expired": parts.append("expired")
        case "superseded": parts.append("replaced")
        default:
            switch memory.trust {
            case "agent_inferred": parts.append("a hint (saved after outside content)")
            case "imported": parts.append("imported")
            default: break
            }
        }
        if let end = memory.endDate { parts.append("until \(end.formatted(date: .abbreviated, time: .omitted))") }
        return parts.joined(separator: " · ")
    }

    static func kindName(_ kind: String?) -> String {
        switch kind {
        case "rule": "Rule"
        case "preference": "Preference"
        case "work_log": "Work log"
        default: "Fact"
        }
    }
}

/// Consolidation ("tidying up"): its mode, what it cost this month, its last run.
private struct ConsolidationSection: View {
    let stats: ConsolidationStats
    let client: AgentClient
    let reload: () async -> Void
    @State private var running = false

    var body: some View {
        Section {
            Picker(
                "Mode",
                selection: Binding(
                    get: { stats.mode },
                    set: { mode in Task { try? await client.setConsolidation(mode: mode); await reload() } })
            ) {
                Text("Off").tag("off")
                Text("Review first").tag("observe")
                Text("Automatic").tag("auto")
            }
            LabeledContent("This month", value: stats.monthCost.formatted(.currency(code: "USD").precision(.fractionLength(2...4))))
            if let last = stats.lastRun {
                LabeledContent("Last run", value: lastRun(last))
            }
            Button {
                running = true
                Task {
                    try? await client.consolidateNow()
                    try? await Task.sleep(for: .seconds(3))
                    await reload()
                    running = false
                }
            } label: {
                HStack {
                    Text("Tidy up now")
                    if running { Spacer(); ProgressView() }
                }
            }
            .disabled(running || stats.mode == "off" || stats.waitingTasks == 0)
        } header: {
            Text("Tidying up")
        } footer: {
            Text(footer)
        }
    }

    private var footer: String {
        let model = stats.model.split(separator: "/").last.map(String.init) ?? stats.model
        let mode = switch stats.mode {
        case "off": "Off: nothing is written after tasks."
        case "auto": "Automatic: after tasks, the agent keeps a work log, saves facts as hints and merges duplicates. New preferences, and anything that would change what you said, wait for your approval."
        default: "Review first: after tasks, the agent keeps a work log; everything else it suggests waits here for your approval."
        }
        let waiting = stats.waitingTasks == 1 ? "1 task waits." : "\(stats.waitingTasks) tasks wait."
        return "\(mode) It runs on \(model), at most \(stats.dailyCapUsd.formatted(.currency(code: "USD"))) a day. \(waiting)"
    }

    private func lastRun(_ run: ConsolidationStats.LastRun) -> String {
        let when = Date(agentTimestamp: run.at)?.formatted(.relative(presentation: .named)) ?? run.at
        guard let counts = run.counts, run.status == "done" else { return "\(when), \(run.status)" }
        var parts: [String] = []
        if counts.saved > 0 { parts.append("\(counts.saved) saved") }
        if counts.proposed > 0 { parts.append("\(counts.proposed) proposed") }
        if counts.merged > 0 { parts.append("\(counts.merged) merged") }
        if counts.expired > 0 { parts.append("\(counts.expired) expired") }
        return parts.isEmpty ? "\(when), nothing new" : "\(when): \(parts.joined(separator: ", "))"
    }
}

/// Edits a memory, and says where it came from.
private struct MemoryEditor: View {
    let client: AgentClient
    let memory: AgentMemory
    let saved: () async -> Void
    @State private var text = ""
    @State private var kind = "fact"
    @State private var ends = false
    @State private var endDate = Date.now.addingTimeInterval(7 * 86_400)
    @State private var saving = false
    @State private var problem: String?
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openTask) private var openTask

    private var changed: Bool {
        let textChanged = text.trimmingCharacters(in: .whitespacesAndNewlines) != memory.text
        let kindChanged = kind != (memory.kind ?? "fact")
        let dateChanged: Bool = switch (ends, memory.endDate) {
        case (false, nil): false
        case (true, let end?): !Calendar.current.isDate(end, inSameDayAs: endDate)
        default: true
        }
        return textChanged || kindChanged || dateChanged
    }

    var body: some View {
        Form {
            Section {
                TextField("What to remember", text: $text, axis: .vertical).lineLimit(2...8)
            } footer: {
                Text("\(text.count)/500")
            }
            Section {
                Picker("Kind", selection: $kind) {
                    Text("Rule").tag("rule")
                    Text("Preference").tag("preference")
                    Text("Fact").tag("fact")
                    Text("Work log").tag("work_log")
                }
                Toggle("Ends on a date", isOn: $ends)
                if ends {
                    DatePicker("Until", selection: $endDate, in: Date.now..., displayedComponents: .date)
                }
            } footer: {
                Text("Rules apply to every task; preferences say how you like things done; facts and the work log are hints. Editing it makes it something you said.")
            }
            Section("Why do you know this?") {
                Text(origin)
                if let run = memory.sourceRun, let openTask {
                    Button("Open that conversation") {
                        dismiss()
                        openTask.open(run)
                    }
                }
            }
            Section {
                if memory.isGone {
                    Button("Restore") { Task { await act { try await client.restore(memory: memory.id) } } }
                } else {
                    if memory.isInferred && !memory.isPending {
                        Button("Confirm it") { Task { await act { try await client.confirm(memory: memory.id) } } }
                    }
                    Button("Forget", role: .destructive) {
                        Task { await act { try await client.forget(memory: memory.id) } }
                    }
                }
            }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle("Memory")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            ToolbarItem(placement: .confirmationAction) {
                Button("Save") { Task { await save() } }
                    .disabled(!changed || saving || text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                        || text.count > 500 || memory.version == nil)
            }
        }
        .onAppear {
            text = memory.text
            kind = memory.kind ?? "fact"
            ends = memory.validUntil != nil
            if let end = memory.endDate { endDate = end }
        }
    }

    private var origin: String {
        let date = Date(agentTimestamp: memory.created)?.formatted(date: .abbreviated, time: .omitted)
            ?? String(memory.created.prefix(10))
        switch (memory.origin, memory.trust) {
        case ("consolidator", _): return "Proposed while the agent tidied up its memory, on \(date)."
        case ("user", _): return "You added or edited it, on \(date)."
        case (_, "imported"): return "Imported on \(date)."
        case (_, "agent_inferred"):
            return "The agent saved it on \(date), after reading a page, an email or command output, so it's a hint, never an instruction."
        default: return "You told the agent in a conversation, on \(date)."
        }
    }

    private func save() async {
        guard let version = memory.version else { return }
        saving = true
        defer { saving = false }
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let until: String?? = if ends {
            .some(Self.day.string(from: endDate))
        } else if memory.validUntil != nil {
            .some(nil)
        } else {
            nil
        }
        do {
            try await client.edit(
                memory: memory.id, text: trimmed == memory.text ? nil : trimmed,
                kind: kind == memory.kind ? nil : kind, validUntil: until, expectedVersion: version)
            await saved()
            dismiss()
        } catch let error as AgentError where error.statusCode == 409 {
            problem = "Changed elsewhere, reloaded."
            await saved()
        } catch let error as AgentError {
            if case .http(_, let message) = error { problem = message }
        } catch {
            problem = error.localizedDescription
        }
    }

    private func act(_ work: () async throws -> Void) async {
        do {
            try await work()
            await saved()
            dismiss()
        } catch {
            problem = error.localizedDescription
        }
    }

    private static let day: DateFormatter = {
        let formatter = DateFormatter()
        formatter.calendar = Calendar(identifier: .gregorian)
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyy-MM-dd"
        return formatter
    }()
}

/// A file to share (an export).
private struct ShareFile: Identifiable {
    let url: URL
    var id: String { url.path() }
}

/// The system share sheet for a file.
private struct ShareSheet: UIViewControllerRepresentable {
    let url: URL

    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: [url], applicationActivities: nil)
    }

    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}
