import AgentAPI
import SwiftUI

/// What the agent remembers between conversations. Memories saved after it
/// read outside content wait for the user to confirm them; any can be forgotten.
struct MemoryView: View {
    let client: AgentClient
    @State private var memories: [AgentMemory] = []
    @State private var loaded = false
    @State private var problem: String?

    private var unverified: [AgentMemory] { memories.filter { !$0.trusted } }
    private var trusted: [AgentMemory] { memories.filter(\.trusted) }

    var body: some View {
        List {
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
            await load()
        } catch {
            problem = error.localizedDescription
        }
    }
}
