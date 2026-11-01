import AgentAPI
import SwiftUI

/// What the agent remembers between conversations. With memory v2 it's
/// grouped by kind, and what waits for the user's approval comes first; with
/// v1, memories saved after outside content wait for a confirm.
struct MemoryView: View {
    let client: AgentClient
    @State private var memories: [AgentMemory] = []
    @State private var loaded = false
    @State private var problem: String?

    /// The desktop runs memory v2 (its memories have kinds).
    private var v2: Bool { memories.contains { $0.kind != nil } }

    var body: some View {
        List {
            if loaded, memories.isEmpty {
                Section {
                    Text("Nothing yet. Tell the agent something to keep, like “Remember that my manager is Lawrence.”")
                        .foregroundStyle(.secondary)
                }
            }
            if v2 { v2Sections } else { v1Sections }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle("Memory")
        .navigationBarTitleDisplayMode(.inline)
        .overlay { if !loaded { ProgressView() } }
        .task { await load() }
        .refreshable { await load() }
    }

    // MARK: Memory v2

    @ViewBuilder private var v2Sections: some View {
        let pending = memories.filter(\.isPending)
        if !pending.isEmpty {
            Section {
                ForEach(pending) { memory in
                    MemoryRow(memory: memory, detail: reason(memory))
                        .swipeActions(edge: .leading) {
                            Button("Approve") { Task { await confirm(memory) } }.tint(.green)
                        }
                        .swipeActions(edge: .trailing) {
                            Button("Reject", role: .destructive) { Task { await reject(memory) } }
                        }
                }
            } header: {
                Text("Needs your approval")
            } footer: {
                Text("The agent doesn't use these until you approve them: swipe right to approve, left to reject.")
            }
        }
        ForEach(
            [("rule", "Rules"), ("preference", "Preferences"), ("fact", "Facts"), ("work_log", "Work log")], id: \.0
        ) { kind, title in
            let items = memories.filter { !$0.isPending && $0.kind == kind }
            if !items.isEmpty {
                Section(title) {
                    ForEach(items) { memory in
                        MemoryRow(memory: memory, detail: source(memory))
                            .swipeActions(edge: .trailing) {
                                Button("Forget", role: .destructive) { Task { await forget(memory) } }
                            }
                            .swipeActions(edge: .leading) {
                                if memory.isInferred {
                                    Button("Confirm") { Task { await confirm(memory) } }.tint(.green)
                                }
                            }
                    }
                }
            }
        }
    }

    private func reason(_ memory: AgentMemory) -> String {
        let kind = memory.kind ?? "fact"
        return switch memory.pendingReason {
        case "tainted_rule": "A \(kind) saved after the agent read a page, an email or command output"
        case "conflict": "Would replace something you told it"
        case "proposed": "Proposed while tidying up memory"
        case "imported": "Imported"
        default: "Waiting for you"
        }
    }

    private func source(_ memory: AgentMemory) -> String {
        let date = String(memory.created.prefix(10))
        return memory.isInferred ? "\(date) · a hint: saved after outside content, swipe right to confirm" : date
    }

    // MARK: Memory v1

    @ViewBuilder private var v1Sections: some View {
        let unverified = memories.filter { !$0.trusted }
        let trusted = memories.filter(\.trusted)
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
            memories = try await client.memories()
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
        loaded = true
    }

    private func forget(_ memory: AgentMemory) async {
        memories.removeAll { $0.id == memory.id }
        do { try await client.forget(memory: memory.id) } catch { await load() }
    }

    private func confirm(_ memory: AgentMemory) async {
        do {
            try await client.confirm(memory: memory.id)
        } catch {
            problem = error.localizedDescription
        }
        await load()
    }

    private func reject(_ memory: AgentMemory) async {
        memories.removeAll { $0.id == memory.id }
        do { try await client.reject(memory: memory.id) } catch { await load() }
    }
}

private struct MemoryRow: View {
    let memory: AgentMemory
    let detail: String

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(memory.text)
            Text(detail).font(.caption).foregroundStyle(.secondary)
        }
        .padding(.vertical, 1)
    }
}
