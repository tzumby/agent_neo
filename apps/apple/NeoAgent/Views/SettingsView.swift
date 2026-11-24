import AgentAPI
import ChatCore
import SwiftUI
import Tailnet

struct SettingsView: View {
    let store: ChatStore
    let client: AgentClient
    @Environment(TailnetService.self) private var tailnet
    @Environment(\.dismiss) private var dismiss
    @State private var confirmingAutoApprove = false

    var body: some View {
        GroupedList {
            if let status = store.status {
                Section {
                    Picker(
                        "Model",
                        selection: Binding(
                            get: { status.settings.model },
                            set: { model in
                                var settings = status.settings
                                settings.model = model
                                Task { await store.update(settings: settings, client: client) }
                            })
                    ) {
                        ForEach(status.knownModels, id: \.self) { Text(shortName($0)).tag($0) }
                    }
                    Toggle(
                        "Browser actions",
                        isOn: Binding(
                            get: { status.settings.allowActions },
                            set: { allow in
                                var settings = status.settings
                                settings.allowActions = allow
                                Task { await store.update(settings: settings, client: client) }
                            })
                    )
                    Toggle(
                        "Auto approve",
                        isOn: Binding(
                            get: { status.settings.autoApprove },
                            set: { on in
                                if on {
                                    confirmingAutoApprove = true
                                } else {
                                    setAutoApprove(false, status)
                                }
                            })
                    )
                    .confirmationDialog(
                        "Turn on Auto approve?", isPresented: $confirmingAutoApprove, titleVisibility: .visible
                    ) {
                        Button("Turn on", role: .destructive) { setAutoApprove(true, status) }
                        Button("Cancel", role: .cancel) {}
                    } message: {
                        Text("Nothing will wait for your OK, including commands as root and changes in your logged-in browser, even if a page or log has fooled the agent.")
                    }
                    LabeledContent("agentd", value: status.version)
                } header: {
                    Text("Agent")
                } footer: {
                    Text("Browser actions on: the agent can type, click and send in the browser, and asks you before each change. Off: it can only read and navigate.\n\nAuto approve on: nothing waits for your OK, in chats or scheduled runs. Keys still come from you, and what the agent is never allowed to do stays refused.")
                }
            }
            Section {
                NavigationLink("Memory") { MemoryView(client: client) }
                NavigationLink("Schedules") { SchedulesView(client: client) }
                NavigationLink("Keys & tokens") { SecretsView(client: client) }
            } footer: {
                Text("What the agent remembers between conversations, the tasks it runs on its own, and the keys it can use without seeing them.")
            }
            Section("Desktop") {
                LabeledContent("Name", value: store.status?.desktop ?? "…")
                LabeledContent("Address", value: client.baseURL.host() ?? "")
            }
            Section("Tailscale") {
                LabeledContent("Signed in as", value: tailnet.status?.loginName ?? "…")
                LabeledContent("This device", value: tailnet.status?.selfNode?.hostName ?? "…")
                Button("Log out of Tailscale", role: .destructive) {
                    dismiss()
                    Task { await tailnet.logOut() }
                }
            }
        }
        .navigationTitle("Settings")
        .inlineTitle()
        .toolbar { Button("Done") { dismiss() } }
    }

    private func setAutoApprove(_ on: Bool, _ status: AgentStatus) {
        var settings = status.settings
        settings.autoApprove = on
        Task { await store.update(settings: settings, client: client) }
    }

    private func shortName(_ model: String) -> String {
        ModelName.display(model)
    }
}
