import AgentAPI
import SwiftUI
import UniformTypeIdentifiers

/// What the agent remembers between conversations (docs/memory-v2.md §14).
/// Nothing here waits for you: you come here only to fix or forget something.
/// With memory v2: the memories by kind, searchable, with expired ones
/// restorable; an edit sheet with "Why do you know this?"; tidying up (on or
/// off) and its cost; export and import. A desktop without memory v2 gets
/// v1's confirm-or-forget list.
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
        GroupedList {
            if v2 { v2Sections } else { v1Sections }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle("Memory")
        .inlineTitle()
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
                .macSheetSize(height: 480)
        }
        #if os(iOS)
            .sheet(item: $sharing) { file in ShareSheet(url: file.url) }
        #else
            // The Mac saves an export where you choose.
            .fileMover(
                isPresented: Binding(get: { sharing != nil }, set: { if !$0 { sharing = nil } }),
                file: sharing?.url
            ) { _ in sharing = nil }
        #endif
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
                (query.isEmpty ? memory.kind == kind : true) && (showGone ? true : !memory.isGone)
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
                            Button("Confirm") { Task { await confirm(memory) } }.tint(.green)
                        }
                    }
                    .macContextMenu {
                        if memory.isGone {
                            Button("Restore") { Task { await restore(memory) } }
                        } else {
                            if memory.isInferred {
                                Button("Confirm") { Task { await confirm(memory) } }
                            }
                            Button("Forget", role: .destructive) { Task { await forget(memory) } }
                        }
                    }
            }
        } footer: {
            if query.isEmpty {
                #if os(macOS)
                    Text("Click one to edit it. Right-click to forget it, to confirm a hint, or to restore an expired one.")
                #else
                    Text("Tap one to edit it. Swipe left to forget it; right to confirm a hint, or to restore an expired one.")
                #endif
            }
        }
        if query.isEmpty, let stats {
            ConsolidationSection(stats: stats, client: client) { await load() }
        }
    }

    private static func empty(_ kind: String) -> String {
        switch kind {
        case "rule": "No rules yet. Tell the agent, like “From now on, reply in English.”"
        case "preference": "No preferences yet, like “I prefer summaries under 100 words.”"
        case "work_log": "No work log yet: after a task that changes something, the agent notes what it did."
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
                            Button("Confirm") { Task { await confirm(memory) } }.tint(.green)
                        }
                        .swipeActions(edge: .trailing) {
                            Button("Forget", role: .destructive) { Task { await forget(memory) } }
                        }
                        .macContextMenu {
                            Button("Confirm") { Task { await confirm(memory) } }
                            Button("Forget", role: .destructive) { Task { await forget(memory) } }
                        }
                }
            } header: {
                Text("To confirm")
            } footer: {
                #if os(macOS)
                    Text("Saved after the agent read a web page, an email or command output, so they could come from that content. It treats them as hints until you confirm them: right-click to confirm or forget.")
                #else
                    Text("Saved after the agent read a web page, an email or command output, so they could come from that content. It treats them as hints until you confirm them: swipe right to confirm, left to forget.")
                #endif
            }
        }
        if !trusted.isEmpty {
            Section("Remembered") {
                ForEach(trusted) { memory in
                    Text(memory.text)
                        .swipeActions(edge: .trailing) {
                            Button("Forget", role: .destructive) { Task { await forget(memory) } }
                        }
                        .macContextMenu {
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
                let statuses = showGone ? ["active", "expired", "superseded"] : ["active"]
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

    /// Vouches for a hint: it becomes something you said.
    private func confirm(_ memory: AgentMemory) async {
        await attempt { try await client.confirm(memory: memory.id) }
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
                ? "Imported \(done.imported). The agent uses them from now on; fix or forget any here."
                : "Imported \(done.imported); skipped \(done.rejected.count) (\(done.rejected.first?.reason ?? ""))."
        } catch {
            problem = error.localizedDescription
        }
        await load()
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
            Text(detail).font(.app(.caption)).foregroundStyle(.secondary)
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

/// Tidying up (consolidation): on or off, what it does, what it cost this
/// month, its last run.
private struct ConsolidationSection: View {
    let stats: ConsolidationStats
    let client: AgentClient
    let reload: () async -> Void
    @State private var running = false

    var body: some View {
        Section {
            Toggle(
                "Tidy up after tasks",
                isOn: Binding(
                    get: { stats.mode != "off" },
                    set: { on in
                        Task {
                            try? await client.setConsolidation(mode: on ? "auto" : "off")
                            await reload()
                        }
                    })
            )
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
        let what = stats.mode == "off"
            ? "Off: nothing is written or merged after tasks. Dated memories still end on their date."
            : "A few minutes after your tasks, a small model notes in the work log what each one changed (a pushed fix, a booked appointment), merges memories that say the same thing, and drops ones that are no longer true. It never changes what you said, never notes what a task only looked up, and skips scheduled runs."
        let waiting = stats.waitingTasks == 1 ? "1 task waits." : "\(stats.waitingTasks) tasks wait."
        let cost = "It runs on \(model), at most \(stats.dailyCapUsd.formatted(.currency(code: "USD"))) a day."
        return stats.mode == "off" ? what : "\(what) \(cost) \(waiting)"
    }

    private func lastRun(_ run: ConsolidationStats.LastRun) -> String {
        let when = Date(agentTimestamp: run.at)?.formatted(.relative(presentation: .named)) ?? run.at
        guard let counts = run.counts, run.status == "done" else { return "\(when), \(run.status)" }
        var parts: [String] = []
        if counts.saved > 0 { parts.append("\(counts.saved) noted") }
        if counts.merged > 0 { parts.append("\(counts.merged) merged") }
        if counts.expired > 0 { parts.append("\(counts.expired) dropped") }
        return parts.isEmpty ? "\(when), nothing to do" : "\(when): \(parts.joined(separator: ", "))"
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
        GroupedForm {
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
                    if memory.isInferred {
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
        .inlineTitle()
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
        case ("consolidator", _): return "Noted while the agent tidied up its memory, on \(date)."
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

#if os(iOS)
    /// The system share sheet for a file.
    private struct ShareSheet: UIViewControllerRepresentable {
        let url: URL

        func makeUIViewController(context: Context) -> UIActivityViewController {
            UIActivityViewController(activityItems: [url], applicationActivities: nil)
        }

        func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
    }
#endif
