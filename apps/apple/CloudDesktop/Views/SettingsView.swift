import AgentAPI
import ChatCore
import SwiftUI
import Tailnet

struct SettingsView: View {
    let store: ChatStore
    let client: AgentClient
    @Environment(TailnetService.self) private var tailnet
    @Environment(\.dismiss) private var dismiss

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
                    LabeledContent("agentd", value: status.version)
                } header: {
                    Text("Agent")
                } footer: {
                    Text("On: the agent can type, click and send in the browser, and asks you before each change. Off: it can only read and navigate.")
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

    private func shortName(_ model: String) -> String {
        model.split(separator: "/").last.map(String.init) ?? model
    }
}
