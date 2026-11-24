import AgentAPI
import ChatCore
import SwiftUI
import Tailnet

struct SettingsView: View {
    let store: ChatStore
    let client: AgentClient
    @Environment(TailnetService.self) private var tailnet
    @Environment(PushCenter.self) private var push
    @Environment(\.dismiss) private var dismiss
    @State private var autoApproveConfirmationVersion: String?
    @State private var loggingOut = false
    /// The desktop didn't confirm it stopped notifying this device.
    @State private var stillRegistered = false
    /// A setting that didn't save (the chat's own alert can't show over this sheet).
    @State private var problem: String?
    private let autoApproveDescription = "Eligible shell commands and schedules can run without asking. Browser changes, saved keys, direct internet access, and changes to the proxy allowlist still require approval. So do commands flagged for possible data transfer after reading outside content."

    var body: some View {
        GroupedList {
            if let status = store.status {
                Section {
                    Picker(
                        "Model",
                        selection: Binding(
                            get: { status.settings.model },
                            set: { model in
                                Task { problem = await store.updateSettings(model: model, client: client) }
                            })
                    ) {
                        ForEach(status.knownModels, id: \.self) { Text(shortName($0)).tag($0) }
                    }
                    Toggle(
                        "Browser actions",
                        isOn: Binding(
                            get: { status.settings.allowActions },
                            set: { allow in
                                Task { problem = await store.updateSettings(allowActions: allow, client: client) }
                            })
                    )
                    Toggle(
                        "Auto approve",
                        isOn: Binding(
                            get: { status.settings.autoApprove },
                            set: { on in
                                if on {
                                    autoApproveConfirmationVersion = status.settings.autoApproveVersion
                                } else {
                                    setAutoApprove(false, version: status.settings.autoApproveVersion)
                                }
                            })
                    )
                    .disabled(!status.settings.autoApprove && status.settings.autoApproveVersion == nil)
                    .confirmationDialog(
                        "Turn on Auto approve?",
                        isPresented: Binding(
                            get: { autoApproveConfirmationVersion != nil },
                            set: { if !$0 { autoApproveConfirmationVersion = nil } }),
                        titleVisibility: .visible,
                        presenting: autoApproveConfirmationVersion
                    ) { version in
                        Button("Turn on", role: .destructive) { setAutoApprove(true, version: version) }
                        Button("Cancel", role: .cancel) {}
                    } message: { _ in
                        Text(autoApproveDescription)
                    }
                    LabeledContent("agentd", value: status.version)
                } header: {
                    Text("Agent")
                } footer: {
                    Text(autoApproveDescription)
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
                Button(role: .destructive) {
                    logOut()
                } label: {
                    HStack {
                        Text(loggingOut ? "Logging out…" : "Log out of Tailscale")
                        if loggingOut {
                            Spacer()
                            ProgressView().controlSize(.small)
                        }
                    }
                }
                .disabled(loggingOut)
            }
            .alert(
                "Notifications may continue",
                // However it's dismissed, signing out goes on.
                isPresented: Binding(
                    get: { stillRegistered },
                    set: { shown in
                        guard !shown, stillRegistered else { return }
                        stillRegistered = false
                        Task { await finishLogOut() }
                    })
            ) {
                Button("Log Out") {}
            } message: {
                Text("Your desktop didn't confirm that it stopped notifying this device. It may keep sending notifications here until you remove this device in the web app, under Settings → Notifications.")
            }
        }
        .navigationTitle("Settings")
        .inlineTitle()
        .toolbar { Button("Done") { dismiss() }.disabled(loggingOut) }
        // Signing out finishes here (its warning shows on this sheet).
        .interactiveDismissDisabled(loggingOut)
        .alert(
            "Settings",
            isPresented: Binding(get: { problem != nil }, set: { if !$0 { problem = nil } })
        ) {
            Button("OK") {}
        } message: {
            Text(problem ?? "")
        }
    }

    private func setAutoApprove(_ enabled: Bool, version: String?) {
        Task { problem = await store.setAutoApprove(enabled, expectedVersion: version, client: client) }
    }

    /// Stops this device's notifications first (bounded, so an unreachable
    /// desktop doesn't hold it up), then signs out of the tailnet.
    private func logOut() {
        loggingOut = true
        Task {
            if await push.revokeBeforeLogout(on: client) {
                await finishLogOut()
            } else {
                stillRegistered = true
            }
        }
    }

    private func finishLogOut() async {
        dismiss()
        await tailnet.logOut()
        push.finishLogout()
    }

    private func shortName(_ model: String) -> String {
        ModelName.display(model)
    }
}
