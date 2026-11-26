import AgentAPI
import ChatCore
import SwiftUI

/// Asks for a provider's API key (agentd 0.20.0): the key the agent thinks
/// with, apart from the keys its commands use. agentd checks it with the
/// provider before keeping it, and uses it from the next message on: no
/// deploy, no restart. Values never come back to the app.
struct ModelKeyForm: View {
    let provider: ProviderRef
    let store: ChatStore
    let client: AgentClient
    /// After the desktop took the key.
    var onSaved: () async -> Void = {}
    /// Outside a form (the chat's banner), where the iPhone draws a field
    /// with no edge at all: a faint capsule shows where to paste.
    var framed = false

    @State private var value = ""
    @State private var pasted = false
    @State private var saving = false
    @State private var problem: String?

    private var key: String { value.trimmingCharacters(in: .whitespacesAndNewlines) }
    private var canSave: Bool { key.count >= 8 && !saving }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                KeyField(prompt: "Paste your \(provider.keyLabel)", value: $value, pasted: $pasted)
                    .onSubmit { if canSave { Task { await save() } } }
                    .padding(.leading, framed ? 14 : 0)
                    .padding(.trailing, framed ? 4 : 0)
                    .padding(.vertical, framed ? 4 : 0)
                    .background(framed ? Theme.chip : .clear, in: .capsule)
                Button {
                    Task { await save() }
                } label: {
                    if saving {
                        ProgressView().controlSize(.small)
                    } else {
                        Text("Save")
                    }
                }
                .primaryButton()
                .disabled(!canSave)
            }
            if let problem {
                Text(problem)
                    .font(.app(.footnote))
                    .foregroundStyle(.red)
                    .lineLimit(1...4)
            }
            Text("Kept only on your desktop and sent only to \(provider.name). Your agent's commands never get it.")
                .font(.app(.footnote))
                .foregroundStyle(.secondary)
                .lineLimit(1...4)
        }
    }

    private func save() async {
        saving = true
        defer { saving = false }
        problem = await store.saveModelKey(key, for: provider, client: client)
        guard problem == nil else { return }
        value = ""
        if pasted { Clipboard.clear() }
        await onSaved()
    }
}

/// A provider's key in a sheet: to add or replace one from Settings, or
/// before switching to a model whose provider has none.
struct ModelKeySheet: View {
    let provider: ProviderRef
    let store: ChatStore
    let client: AgentClient
    /// Why it's asked for, above the field ("Claude Opus 5.5 needs it.").
    var reason: String? = nil
    var onSaved: () async -> Void = {}

    @Environment(\.dismiss) private var dismiss

    var body: some View {
        GroupedForm {
            Section {
                ModelKeyForm(provider: provider, store: store, client: client) {
                    await onSaved()
                    dismiss()
                }
            } header: {
                if let reason { Text(reason) }
            }
        }
        .navigationTitle(provider.keyLabel)
        .inlineTitle()
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("Cancel") { dismiss() }
            }
        }
    }
}

/// Each provider and where its key comes from: pasted in an app, the
/// desktop's setup (the deployment), or none yet. Add or replace one; remove
/// one pasted in an app (the deployment's, if any, takes over again).
struct ModelKeysView: View {
    let store: ChatStore
    let client: AgentClient

    @State private var editing: ProviderRef?
    @State private var removing: ProviderRef?
    @State private var problem: String?

    private var providers: [ModelProvider] { store.status?.modelProviders ?? [] }

    var body: some View {
        GroupedList {
            Section {
                ForEach(providers) { provider in
                    HStack(spacing: 10) {
                        VStack(alignment: .leading, spacing: 3) {
                            Text(provider.ref.keyLabel).font(.app(.headline))
                            Text(Self.source(of: provider)).font(.app(.caption)).foregroundStyle(.secondary)
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        // Borderless: in an iPhone list, a row's plain buttons all fire on a tap.
                        if provider.key == "app" {
                            Button("Remove", role: .destructive) { removing = provider.ref }
                                .buttonStyle(.borderless)
                        }
                        Button(provider.hasKey ? "Replace" : "Add") { editing = provider.ref }
                            .buttonStyle(.borderless)
                    }
                    .padding(.vertical, 2)
                }
            } footer: {
                Text("The keys your agent thinks with. A task uses its model's provider, so switching is picking another model in Settings. A key pasted here replaces the desktop's own until you remove it. Removing doesn't revoke a key: do that in the provider's console.")
                    .sectionFooter()
            }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle("Model keys")
        .inlineTitle()
        .sheet(item: $editing) { provider in
            NavigationStack { ModelKeySheet(provider: provider, store: store, client: client) }
                .macSheetSize(height: 300)
        }
        .confirmationDialog(
            "Remove the \(removing?.keyLabel ?? "key")?",
            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
            titleVisibility: .visible,
            presenting: removing
        ) { provider in
            Button("Remove", role: .destructive) {
                Task { problem = await store.removeModelKey(for: provider, client: client) }
            }
            Button("Cancel", role: .cancel) {}
        } message: { provider in
            Text("Your agent stops using it. If the desktop's setup has a key for \(provider.name), that one takes over; if not, the chat asks for a new one.")
        }
    }

    static func source(of provider: ModelProvider) -> String {
        switch provider.key {
        case "app": "Saved in the app"
        case "deployment": "From the deployment"
        case nil: "No key"
        case let other?: other
        }
    }
}

/// The chat model's provider has no key (a new desktop, or its key was
/// removed): the chat asks for it above the composer, so the first message
/// can go.
struct ModelKeyBanner: View {
    let provider: ProviderRef
    let store: ChatStore
    let client: AgentClient

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                Image(systemName: "key.fill").foregroundStyle(Theme.amber)
                // Wraps at the width it gets (not fixedSize: see BudgetBanner).
                Text("Your agent needs your \(provider.keyLabel) to work.")
                    .font(.app(.subheadline).weight(.medium))
                    .lineLimit(1...3)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            ModelKeyForm(provider: provider, store: store, client: client, framed: true)
        }
        .padding(14)
        .floating(.rect(cornerRadius: 22))
        .accessibilityElement(children: .contain)
    }
}
