import AgentAPI
import ChatCore
import SwiftUI

struct ChatItemView: View {
    let item: ChatItem
    let store: ChatStore
    let client: AgentClient
    /// Opens the take-over view; nil while the desktop can't be reached.
    var onTakeOver: ((Action) -> Void)?

    var body: some View {
        switch item {
        case .user(let message):
            HStack {
                Spacer(minLength: 48)
                Text(message.text)
                    .padding(12)
                    .foregroundStyle(.white)
                    .background(Color.accentColor, in: RoundedRectangle(cornerRadius: 18))
            }
        case .assistant(let message):
            HStack {
                Text(markdown(message.text))
                    .textSelection(.enabled)
                    .padding(12)
                    .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 18))
                Spacer(minLength: 32)
            }
        case .work(let work):
            WorkRow(work: work)
        case .approval(let approval):
            ApprovalCard(approval: approval, store: store, client: client)
        case .action(let action):
            ActionCard(action: action, store: store, client: client, onTakeOver: onTakeOver)
        case .image(let picture):
            VStack(alignment: .leading, spacing: 6) {
                RemoteImage(path: picture.path, client: client)
                if let caption = picture.caption, !caption.isEmpty {
                    Text(caption).font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }
}

private func markdown(_ text: String) -> AttributedString {
    let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
    return (try? AttributedString(markdown: text, options: options)) ?? AttributedString(text)
}

// MARK: - Work (tool steps)

private struct WorkRow: View {
    let work: Work

    var body: some View {
        DisclosureGroup {
            VStack(alignment: .leading, spacing: 6) {
                ForEach(work.steps) { step in StepRow(step: step) }
            }
            .padding(.top, 6)
        } label: {
            HStack(spacing: 8) {
                if work.finished == nil {
                    ProgressView().controlSize(.small)
                } else {
                    Image(systemName: icon).foregroundStyle(.secondary)
                }
                Text(summary).font(.subheadline).foregroundStyle(.secondary)
            }
        }
        .tint(.secondary)
    }

    private var icon: String {
        switch work.finished?.status {
        case "done": "checkmark.circle"
        case "needs_user": "hand.raised"
        case "cancelled": "stop.circle"
        default: "exclamationmark.triangle"
        }
    }

    private var summary: String {
        let steps = work.steps.count == 1 ? "1 step" : "\(work.steps.count) steps"
        guard let finished = work.finished else { return "Working · \(steps)" }
        var parts = ["\(finished.steps == 1 ? "1 step" : "\(finished.steps) steps")", "\(Int(finished.seconds))s"]
        if let cost = finished.cost { parts.append(String(format: "$%.3f", cost)) }
        switch finished.status {
        case "needs_user": parts.insert("Waiting for you", at: 0)
        case "cancelled": parts.insert("Stopped", at: 0)
        case "error": parts.insert("Failed", at: 0)
        case "max_steps": parts.insert("Gave up", at: 0)
        default: break
        }
        return parts.joined(separator: " · ")
    }
}

private struct StepRow: View {
    let step: Work.Step

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: icon).frame(width: 18).foregroundStyle(.secondary)
            if step.tool == "run_shell" {
                Text("$ \(step.detail)").font(.caption.monospaced()).lineLimit(3)
                if step.asRoot {
                    Text("root").font(.caption2.bold()).padding(.horizontal, 5).padding(.vertical, 1)
                        .background(.orange.opacity(0.2), in: Capsule())
                }
            } else {
                Text("\(step.tool) \(compact(step.detail))").font(.caption.monospaced()).lineLimit(2)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var icon: String {
        switch step.tool {
        case "run_shell": "terminal"
        case "open_url": "globe"
        case "read_text": "doc.text"
        case "snapshot": "list.bullet.rectangle"
        case "click": "cursorarrow.click"
        case "type_text", "press_key": "keyboard"
        case "list_tabs", "switch_tab": "square.on.square"
        case "show_screenshot": "photo"
        default: "gearshape"
        }
    }

    private func compact(_ args: String) -> String {
        args == "{}" ? "" : args
    }
}

// MARK: - Approval

private struct ApprovalCard: View {
    let approval: Approval
    let store: ChatStore
    let client: AgentClient

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("Run this command?", systemImage: "exclamationmark.shield").font(.headline)
            Text(approval.command)
                .font(.callout.monospaced())
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color(.tertiarySystemBackground), in: RoundedRectangle(cornerRadius: 10))
            Text("It \(approval.reason).").font(.footnote).foregroundStyle(.secondary)
            switch approval.state {
            case .pending:
                HStack {
                    Spacer()
                    Button("Deny", role: .destructive) {
                        Task { await store.answer(approval, approved: false, client: client) }
                    }
                    .buttonStyle(.bordered)
                    Button("Approve") {
                        Task { await store.answer(approval, approved: true, client: client) }
                    }
                    .buttonStyle(.borderedProminent)
                }
            case .approved: Label("Approved", systemImage: "checkmark").font(.subheadline).foregroundStyle(.green)
            case .denied: Label("Denied", systemImage: "xmark").font(.subheadline).foregroundStyle(.red)
            case .expired: Text("Expired").font(.subheadline).foregroundStyle(.secondary)
            }
        }
        .padding(14)
        .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 18))
    }
}

// MARK: - Action needed (Grok-style login handoff)

private struct ActionCard: View {
    let action: Action
    let store: ChatStore
    let client: AgentClient
    let onTakeOver: ((Action) -> Void)?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("Computer", systemImage: "laptopcomputer").font(.subheadline.weight(.semibold))
                .foregroundStyle(.secondary)
            Text("Action needed").font(.title3.bold())
            Text(action.reason)
            if let path = action.imagePath {
                RemoteImage(path: path, client: client)
            }
            if let title = pageTitle {
                Text(title).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            switch action.state {
            case .pending:
                HStack {
                    Spacer()
                    Button("Skip") { Task { await store.resolve(action, done: false, client: client) } }
                        .buttonStyle(.bordered)
                    Button("I'm done") { Task { await store.resolve(action, done: true, client: client) } }
                        .buttonStyle(.bordered)
                    Button("Take over") { onTakeOver?(action) }
                        .buttonStyle(.borderedProminent)
                        .disabled(onTakeOver == nil)
                }
            case .done: Label("Done", systemImage: "checkmark").font(.subheadline).foregroundStyle(.green)
            case .skipped: Text("Skipped").font(.subheadline).foregroundStyle(.secondary)
            case .expired: Text("Expired").font(.subheadline).foregroundStyle(.secondary)
            }
            Label(
                "If you sign in, you do it in the browser on your desktop, and the session stays signed in there. The site may email you about a new device.",
                systemImage: "checkmark.shield"
            )
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(14)
        .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 18))
    }

    private var pageTitle: String? {
        let title = action.page.components(separatedBy: " | ").first ?? ""
        return title.isEmpty ? nil : title
    }
}

// MARK: - Images from the desktop

/// Loads /api/v1/images/<id> through the tailnet session (AsyncImage can't use it).
struct RemoteImage: View {
    let path: String
    let client: AgentClient
    @State private var image: UIImage?
    @State private var failed = false

    var body: some View {
        Group {
            if let image {
                Image(uiImage: image).resizable().scaledToFit()
            } else if failed {
                Label("Image unavailable", systemImage: "photo").font(.caption).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 60)
            } else {
                ProgressView().frame(maxWidth: .infinity, minHeight: 120)
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .task(id: path) {
            do {
                image = UIImage(data: try await client.imageData(path: path))
                failed = image == nil
            } catch {
                failed = true
            }
        }
    }
}
