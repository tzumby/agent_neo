import AgentAPI
import SwiftUI
import Tailnet

/// Phase 2: proves the whole path works. Tailscale identity, the desktop found
/// on the tailnet, and agentd's status fetched through it.
struct StatusView: View {
    @Environment(TailnetService.self) private var tailnet
    @State private var agent: AgentStatus?
    @State private var agentError: String?
    @State private var loading = false

    private var desktop: TailnetStatus.Peer? { tailnet.desktops.first }

    var body: some View {
        List {
            Section("Tailscale") {
                LabeledContent("Signed in as", value: tailnet.status?.loginName ?? "…")
                LabeledContent("This device", value: tailnet.status?.selfNode?.hostName ?? "…")
                LabeledContent("Tailnet", value: tailnet.status?.currentTailnet?.magicDNSSuffix ?? "…")
            }
            Section("Desktop") {
                if let desktop {
                    LabeledContent("Name", value: desktop.hostName)
                    LabeledContent("Online", value: desktop.online == true ? "Yes" : "No")
                } else {
                    Text("No device tagged tag:cloud-desktop on your tailnet.")
                        .foregroundStyle(.secondary)
                }
            }
            Section {
                if let agent {
                    LabeledContent("Model", value: agent.settings.model)
                    LabeledContent("Busy", value: agent.busy ? "Yes" : "No")
                    LabeledContent("Browser actions", value: agent.settings.allowActions ? "On" : "Read-only")
                    LabeledContent("Events", value: "\(agent.lastSeq)")
                    LabeledContent("agentd", value: agent.version)
                } else if let agentError {
                    Text(agentError).foregroundStyle(.red)
                } else if loading {
                    ProgressView()
                }
            } header: {
                Text("Agent")
            }
            Section {
                Button("Log out of Tailscale", role: .destructive) {
                    Task { await tailnet.logOut() }
                }
            }
        }
        .navigationTitle(desktop?.hostName ?? "Cloud Desktop")
        .refreshable { await loadAgent() }
        .task(id: desktop?.host) { await loadAgent() }
    }

    private func loadAgent() async {
        guard let desktop, let session = tailnet.session else { return }
        loading = true
        defer { loading = false }
        do {
            agent = try await AgentClient(host: desktop.host, session: session).status()
            agentError = nil
        } catch {
            agentError = error.localizedDescription
        }
    }
}
