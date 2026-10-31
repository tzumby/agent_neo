import AgentAPI
import ChatCore
import SwiftUI

struct ChatView: View {
    let store: ChatStore
    let client: AgentClient
    let desktopName: String
    let connectionKey: String

    @State private var draft = ""
    @State private var showingSettings = false

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    if store.timeline.items.isEmpty, store.connection == .live {
                        EmptyChat()
                    }
                    ForEach(store.timeline.items) { item in
                        ChatItemView(item: item, store: store, client: client)
                            .id(item.id)
                    }
                    if store.busy {
                        ProgressView().padding(.leading, 4).id("busy")
                    }
                }
                .padding()
            }
            .scrollDismissesKeyboard(.interactively)
            .onChange(of: store.timeline.items) { scrollToEnd(proxy) }
            .onChange(of: store.busy) { scrollToEnd(proxy) }
            .onAppear { scrollToEnd(proxy, animated: false) }
        }
        .safeAreaInset(edge: .bottom) { composer }
        .navigationTitle(desktopName)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarLeading) { ConnectionBadge(connection: store.connection) }
            ToolbarItem(placement: .topBarTrailing) {
                Button("Settings", systemImage: "gearshape") { showingSettings = true }
            }
        }
        .sheet(isPresented: $showingSettings) {
            NavigationStack { SettingsView(store: store, client: client) }
        }
        .alert(
            "Cloud Desktop",
            isPresented: Binding(get: { store.notice != nil }, set: { if !$0 { store.notice = nil } })
        ) {
            Button("OK") {}
        } message: {
            Text(store.notice ?? "")
        }
        .task(id: connectionKey) { await store.run(client: client) }
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextField("Ask your desktop", text: $draft, axis: .vertical)
                .lineLimit(1...6)
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 20))
            if store.busy {
                Button("Stop", systemImage: "stop.circle.fill") {
                    Task { await store.stop(client: client) }
                }
                .labelStyle(.iconOnly)
                .font(.system(size: 32))
                .tint(.red)
            } else {
                Button("Send", systemImage: "arrow.up.circle.fill", action: send)
                    .labelStyle(.iconOnly)
                    .font(.system(size: 32))
                    .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || store.connection != .live)
            }
        }
        .padding(.horizontal)
        .padding(.vertical, 8)
        .background(.bar)
    }

    private func send() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        draft = ""
        Task { await store.send(text, client: client) }
    }

    private func scrollToEnd(_ proxy: ScrollViewProxy, animated: Bool = true) {
        guard let last = store.busy ? "busy" : store.timeline.items.last?.id else { return }
        if animated {
            withAnimation { proxy.scrollTo(last, anchor: .bottom) }
        } else {
            proxy.scrollTo(last, anchor: .bottom)
        }
    }
}

private struct EmptyChat: View {
    var body: some View {
        ContentUnavailableView(
            "Your desktop is ready",
            systemImage: "sparkles",
            description: Text("Ask it to check your email, look something up, or run a command.")
        )
        .padding(.top, 80)
    }
}

private struct ConnectionBadge: View {
    let connection: ChatStore.Connection

    var body: some View {
        switch connection {
        case .live:
            Image(systemName: "circle.fill").foregroundStyle(.green).font(.caption2)
                .accessibilityLabel("Connected")
        case .connecting:
            ProgressView().controlSize(.small)
        case .offline(let reason):
            Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange)
                .accessibilityLabel("Offline: \(reason)")
        }
    }
}
