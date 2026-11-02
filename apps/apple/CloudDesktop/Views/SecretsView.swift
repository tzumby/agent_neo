import AgentAPI
import ChatCore
import SwiftUI

/// Keys and tokens saved on the desktop (a GitHub token, an API key). The
/// agent asks for one in the chat when a job needs it; they can also be added,
/// replaced and deleted here. Values never come back to the phone.
struct SecretsView: View {
    let client: AgentClient
    @State private var secrets: [AgentSecret] = []
    @State private var loaded = false
    @State private var problem: String?
    @State private var editing: SecretEditor.Target?

    var body: some View {
        GroupedList {
            if loaded, secrets.isEmpty {
                Section {
                    Text("No keys yet. When a job needs one, like a GitHub token to clone a private repository, the agent asks for it in the chat. You can also add one here.")
                        .foregroundStyle(.secondary)
                }
            }
            if !secrets.isEmpty {
                Section {
                    ForEach(secrets) { secret in
                        Button { editing = .replace(secret) } label: { SecretRow(secret: secret) }
                            .foregroundStyle(.primary)
                            .swipeActions(edge: .trailing) {
                                Button("Delete", role: .destructive) { Task { await delete(secret) } }
                            }
                            .macContextMenu {
                                Button("Replace…") { editing = .replace(secret) }
                                Button("Delete", role: .destructive) { Task { await delete(secret) } }
                            }
                    }
                } footer: {
                    #if os(macOS)
                        Text("Keys stay on your desktop and never come back to this Mac. The agent gives one to a command by name and never sees its value; it's hidden from the chat and notifications. Click a key to replace it, right-click to delete it. Deleting doesn't revoke it: do that where you created it.")
                    #else
                        Text("Keys stay on your desktop and never come back to this phone. The agent gives one to a command by name and never sees its value; it's hidden from the chat and notifications. Tap a key to replace it, swipe to delete it. Deleting doesn't revoke it: do that where you created it.")
                    #endif
                }
            }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle("Keys & tokens")
        .inlineTitle()
        .toolbar {
            Button("Add a key", systemImage: "plus") { editing = .new(nil) }
        }
        .sheet(item: $editing) { target in
            NavigationStack {
                SecretEditor(client: client, target: target) { _ in await load() }
            }
            .macSheetSize(height: 420)
        }
        .overlay { if !loaded { ProgressView() } }
        .task { await load() }
        .refreshable { await load() }
    }

    private func load() async {
        do {
            secrets = try await client.secrets()
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
        loaded = true
    }

    private func delete(_ secret: AgentSecret) async {
        secrets.removeAll { $0.name == secret.name }
        do { try await client.deleteSecret(name: secret.name) } catch { await load() }
    }
}

private struct SecretRow: View {
    let secret: AgentSecret

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(secret.label).font(.headline)
            Text(secret.name).font(.caption.monospaced()).foregroundStyle(.secondary)
            if let detail {
                Text(detail).font(.caption).foregroundStyle(.tertiary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(.rect)
        .padding(.vertical, 2)
    }

    private var detail: String? {
        var parts: [String] = []
        if let added = Date(agentTimestamp: secret.created) {
            parts.append("Added \(added.formatted(.dateTime.month().day()))")
        }
        if let used = secret.lastUsed.flatMap(Date.init(agentTimestamp:)) {
            parts.append("used \(used.formatted(.relative(presentation: .named)))")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }
}

/// Adds a key, or replaces one's value. Also opened from the chat when a
/// message looks like it holds a key.
struct SecretEditor: View {
    enum Target: Identifiable {
        /// A new key, perhaps found in a message the user was about to send.
        case new(KeyDetector.Found?)
        case replace(AgentSecret)

        var id: String {
            switch self {
            case .new(let found): "new-\(found?.name ?? "")"
            case .replace(let secret): "replace-\(secret.name)"
            }
        }
    }

    let client: AgentClient
    let target: Target
    /// After saving, with the key's name.
    let onSaved: (String) async -> Void

    @State private var name = ""
    @State private var label = ""
    @State private var value = ""
    @State private var pasted = false
    @State private var saving = false
    @State private var problem: String?
    @Environment(\.dismiss) private var dismiss

    private var isNew: Bool {
        if case .new = target { true } else { false }
    }

    private var canSave: Bool {
        KeyDetector.isValidName(name) && value.trimmingCharacters(in: .whitespacesAndNewlines).count >= 8 && !saving
    }

    var body: some View {
        GroupedForm {
            if isNew {
                Section {
                    TextField("Name, like GITHUB_TOKEN", text: $name)
                        .font(.body.monospaced())
                        #if os(iOS)
                            .textInputAutocapitalization(.characters)
                        #endif
                        .autocorrectionDisabled()
                        .onChange(of: name) { _, new in
                            let normalized = KeyDetector.normalizedName(new)
                            if normalized != new { name = normalized }
                        }
                    TextField("What it is, like GitHub token", text: $label)
                } footer: {
                    Text("Commands get it as $\(name.isEmpty ? "NAME" : name). Keys only: never a password.")
                }
            } else {
                Section {
                    LabeledContent(label, value: name).font(.body.monospaced())
                }
            }
            Section {
                KeyField(prompt: "Key or token", value: $value, pasted: $pasted)
            } footer: {
                Text("Saved only on your desktop, over your tailnet. This \(Device.noun) can't read it back.")
            }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle(isNew ? "Add a key" : "Replace key")
        .inlineTitle()
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("Cancel") { dismiss() }
            }
            ToolbarItem(placement: .confirmationAction) {
                Button("Save") { Task { await save() } }.disabled(!canSave)
            }
        }
        .onAppear(perform: fill)
    }

    private func fill() {
        switch target {
        case .new(let found):
            name = found?.name ?? ""
            label = found?.label ?? ""
            value = found?.value ?? ""
        case .replace(let secret):
            name = secret.name
            label = secret.label
        }
    }

    private func save() async {
        saving = true
        defer { saving = false }
        let label = label.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            _ = try await client.saveSecret(
                name: name, value: value, label: label.isEmpty ? nil : label, approval: nil)
            if pasted { Clipboard.clear() }
            await onSaved(name)
            dismiss()
        } catch let error as AgentError {
            if case .http(400, let message) = error { problem = message } else { problem = error.localizedDescription }
        } catch {
            problem = error.localizedDescription
        }
    }
}

/// A secure field with a Paste button (which pastes without iOS asking for
/// permission). `pasted`: the value came from the clipboard, so it can be
/// cleared from there once saved.
struct KeyField: View {
    let prompt: String
    @Binding var value: String
    @Binding var pasted: Bool

    var body: some View {
        HStack(spacing: 8) {
            SecureField(prompt, text: $value)
                #if os(iOS)
                    .textInputAutocapitalization(.never)
                #endif
                .autocorrectionDisabled()
            PasteButton(payloadType: String.self) { strings in
                guard let text = strings.first else { return }
                Task { @MainActor in
                    value = text.trimmingCharacters(in: .whitespacesAndNewlines)
                    pasted = true
                }
            }
            .labelStyle(.iconOnly)
            .buttonBorderShape(.capsule)
        }
    }
}

extension Date {
    /// agentd's timestamps: "2026-11-01T16:07:12.345Z".
    init?(agentTimestamp text: String) {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        guard let date = formatter.date(from: text) ?? ISO8601DateFormatter().date(from: text) else { return nil }
        self = date
    }
}
