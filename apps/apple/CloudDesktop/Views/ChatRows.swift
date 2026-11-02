import AgentAPI
import ChatCore
import SwiftUI

/// One item of the chat: a message, a picture, or a card waiting for the
/// user. Its only input is the item, so SwiftUI skips rows that didn't change
/// (see the environment values in Theme.swift).
struct ChatRow: View, Equatable {
    let item: ChatItem

    var body: some View {
        switch item {
        case .user(let message): UserBubble(text: message.text, schedule: message.schedule)
        case .assistant(let message):
            if message.quiet { QuietMessage(text: message.text) } else { AssistantMessage(text: message.text) }
        case .approval(let approval):
            if let request = approval.secret {
                SecretRequestCard(approval: approval, request: request)
            } else if let proposal = approval.schedule {
                ScheduleApprovalCard(approval: approval, proposal: proposal)
            } else if let action = approval.browser {
                BrowserApprovalCard(approval: approval, action: action)
            } else {
                ApprovalCard(approval: approval)
            }
        case .action(let action): ActionCard(action: action)
        case .image(let picture): PictureRow(picture: picture)
        }
    }
}

// MARK: - Messages

private struct UserBubble: View {
    let text: String
    /// Set when the desktop sent this itself: a scheduled task's run.
    let schedule: String?

    var body: some View {
        VStack(alignment: .trailing, spacing: 6) {
            if let schedule {
                Label("Scheduled · \(schedule)", systemImage: "clock")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Text(text)
                .padding(.horizontal, 16)
                .padding(.vertical, 11)
                .background(Theme.userBubble, in: .rect(cornerRadius: 22))
                .contextMenu {
                    Button("Copy", systemImage: "doc.on.doc") { Clipboard.copy(text) }
                }
        }
        .frame(maxWidth: .infinity, alignment: .trailing)
        .padding(.leading, 56)
    }
}

/// The agent's answer: Markdown on the background, as in Grok, with code
/// blocks and tables as cards.
private struct AssistantMessage: View {
    let text: String

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            MarkdownView(text: text)
            CopyButton(text: text).font(.footnote)
        }
    }
}

/// A scheduled run's answer when it found nothing new: one quiet line, the
/// text on a tap. (Its phone stayed quiet too.)
private struct QuietMessage: View {
    let text: String
    @State private var open = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Button {
                withAnimation(.snappy(duration: 0.2)) { open.toggle() }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: "moon.zzz").font(.footnote)
                    Text("Nothing new").font(.subheadline)
                    Image(systemName: "chevron.right")
                        .font(.caption2.weight(.bold))
                        .rotationEffect(.degrees(open ? 90 : 0))
                }
                .foregroundStyle(.secondary)
                .contentShape(.rect)
            }
            .buttonStyle(.plain)
            if open {
                MarkdownView(text: text).foregroundStyle(.secondary)
            }
        }
    }
}

/// Copies text, with a checkmark for a moment afterwards.
struct CopyButton: View {
    let text: String
    @State private var copied = false

    var body: some View {
        Button {
            Clipboard.copy(text)
            copied = true
            Task {
                try? await Task.sleep(for: .seconds(1.5))
                copied = false
            }
        } label: {
            Image(systemName: copied ? "checkmark" : "doc.on.doc")
                .contentTransition(.symbolEffect(.replace))
                .frame(width: 28, height: 28)
                .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .foregroundStyle(.secondary)
        .accessibilityLabel("Copy")
        .sensoryFeedback(.success, trigger: copied) { _, new in new }
    }
}

// MARK: - Approval

/// A command that needs the user's OK: what it does in plain words and why
/// it needs the OK; the command itself only when asked for.
private struct ApprovalCard: View {
    let approval: Approval
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client
    @State private var showCommand = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label {
                Text("Needs your OK").font(.subheadline.weight(.semibold))
            } icon: {
                Image(systemName: "exclamationmark.shield.fill")
            }
            .foregroundStyle(.orange)
            Text(approval.summary ?? "Run a command on your desktop").font(.headline)
            Text("It \(approval.reason).").font(.subheadline).foregroundStyle(.secondary)
            if approval.tainted {
                Label(
                    "The agent read web pages or other outside content before asking. Check the command if this isn't what you asked for.",
                    systemImage: "exclamationmark.triangle.fill"
                )
                .font(.caption)
                .foregroundStyle(.orange)
            }
            Button {
                withAnimation(.snappy(duration: 0.2)) { showCommand.toggle() }
            } label: {
                HStack(spacing: 4) {
                    Text(showCommand ? "Hide the command" : "Show the command")
                    Image(systemName: "chevron.right")
                        .font(.caption2.weight(.bold))
                        .rotationEffect(.degrees(showCommand ? 90 : 0))
                }
                .font(.footnote.weight(.medium))
                .foregroundStyle(.secondary)
                .contentShape(.rect)
            }
            .buttonStyle(.plain)
            if showCommand {
                Text(approval.command)
                    .font(.footnote.monospaced())
                    .textSelection(.enabled)
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Theme.inset, in: .rect(cornerRadius: 14))
                    .transition(.opacity)
            }
            // Shown only while it waits: answered, it leaves the chat.
            HStack(spacing: 10) {
                Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { answer(true) } label: {
                    Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                }
                .primaryGlass()
            }
            .controlSize(.large)
        }
        .card()
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        Task { await store.answer(approval, approved: approved, client: client) }
    }
}

/// "Write this?": the agent wants to type, press a key or click something
/// that may send or change things in the desktop's browser.
/// A keyboard key, drawn like a keycap.
private struct KeyCap: View {
    let key: String

    var body: some View {
        Text(key)
            .font(.callout.monospaced().weight(.semibold))
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(.secondary.opacity(0.6), lineWidth: 1))
    }
}

private struct BrowserApprovalCard: View {
    let approval: Approval
    let action: BrowserAction
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: icon).foregroundStyle(.orange)
                Text(title).font(.headline)
                Spacer()
                if !action.site.isEmpty {
                    Label(site, systemImage: "globe").font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            // What the agent would do, shown as a quote or a key, not a box that
            // looks like a text field: the card asks for a yes or a no.
            switch action.action {
            case "type":
                if !action.target.isEmpty {
                    Text("In “\(action.target)”").font(.footnote).foregroundStyle(.secondary).lineLimit(2)
                }
                HStack(alignment: .top, spacing: 10) {
                    Capsule().fill(.orange.opacity(0.6)).frame(width: 3)
                    Text(action.text ?? "")
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .fixedSize(horizontal: false, vertical: true)
            case "key":
                HStack(spacing: 8) {
                    KeyCap(key: action.key ?? "?")
                    if !action.target.isEmpty {
                        Text("in “\(action.target)”").foregroundStyle(.secondary).lineLimit(2)
                    }
                }
                .font(.callout)
            default:
                Text("“\(action.target)”").font(.callout.weight(.medium)).lineLimit(3)
            }
            if action.action == "type", action.submit {
                Label("Then presses Enter, which may send it", systemImage: "return")
                    .font(.footnote)
                    .foregroundStyle(.orange)
            } else if action.action == "type", action.sends {
                Label("Approving also lets it send this", systemImage: "paperplane")
                    .font(.footnote)
                    .foregroundStyle(.orange)
            } else if action.action != "type", !approval.reason.isEmpty {
                Text("It \(approval.reason).").font(.footnote).foregroundStyle(.secondary)
            }
            // Shown only while it waits: answered, it leaves the chat.
            HStack(spacing: 10) {
                Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { answer(true) } label: {
                    Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                }
                .primaryGlass()
            }
            .controlSize(.large)
        }
        .card()
    }

    private var title: String {
        switch action.action {
        case "type": action.submit || action.sends ? "Write and send this?" : "Write this?"
        case "key": "Press \(action.key ?? "a key")?"
        default: "Click this?"
        }
    }

    private var icon: String {
        switch action.action {
        case "type": "keyboard"
        case "key": action.key == "Enter" ? "return" : "keyboard"
        default: "cursorarrow.click.2"
        }
    }


    /// app.slack.com → slack.com
    private var site: String {
        var host = action.site
        for prefix in ["www.", "app.", "mail."] where host.hasPrefix(prefix) {
            host.removeFirst(prefix.count)
        }
        return host
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        Task { await store.answer(approval, approved: approved, client: client) }
    }
}

private struct Outcome: View {
    let text: String
    let icon: String
    let color: Color

    var body: some View {
        Label(text, systemImage: icon).font(.subheadline.weight(.medium)).foregroundStyle(color)
    }
}

// MARK: - Schedules

/// "Schedule this?": asked for after the agent read a page, an email or
/// command output, so the request could come from that content.
private struct ScheduleApprovalCard: View {
    let approval: Approval
    let proposal: ScheduleProposal
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label {
                Text("Schedule this?").font(.headline)
            } icon: {
                Image(systemName: "calendar.badge.exclamationmark").foregroundStyle(.orange)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text(proposal.name).font(.subheadline.weight(.semibold))
                Text(proposal.when).font(.subheadline)
                Text(proposal.prompt)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(8)
                    .textSelection(.enabled)
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.inset, in: .rect(cornerRadius: 14))
            Text("The agent asked for this after reading outside content. Approve only if you asked for it.")
                .font(.footnote)
                .foregroundStyle(.secondary)
            HStack(spacing: 10) {
                Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { answer(true) } label: {
                    Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                }
                .primaryGlass()
            }
            .controlSize(.large)
        }
        .card()
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        Task { await store.answer(approval, approved: approved, client: client) }
    }
}

// MARK: - Keys and tokens

/// The agent asks for a key or token: why, how to create it, a link, and a
/// secure field. The value goes to the desktop over the tailnet; the chat and
/// the model only ever see its name.
private struct SecretRequestCard: View {
    let approval: Approval
    let request: SecretRequest
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client
    @State private var value = ""
    @State private var pasted = false
    @State private var saving = false

    private var ready: Bool { value.trimmingCharacters(in: .whitespacesAndNewlines).count >= 8 && !saving }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label {
                Text(request.replace ? "A new \(request.label)" : request.label).font(.headline)
            } icon: {
                Image(systemName: "key.fill").foregroundStyle(.yellow)
            }
            if !request.why.isEmpty {
                Text(request.why)
            }
            if request.saved {
                // Saved while the desktop was busy: carry on when it's free.
                HStack {
                    Outcome(text: "Saved on your desktop", icon: "lock.fill", color: .green)
                    Spacer()
                    Button("Continue") { answer(true) }.glassButton()
                }
            } else {
                form
            }
        }
        .card()
    }

    @ViewBuilder private var form: some View {
        if !request.how.isEmpty {
            MarkdownView(text: request.how)
                .font(.subheadline)
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.inset, in: .rect(cornerRadius: 14))
        }
        if let url = request.url {
            Link(destination: url) {
                Label("Open \(request.host ?? url.host() ?? "the page")", systemImage: "arrow.up.right")
                    .frame(maxWidth: .infinity)
            }
            .glassButton()
            .controlSize(.large)
        }
        KeyField(prompt: "Paste your \(request.label)", value: $value, pasted: $pasted)
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            .background(Theme.inset, in: .rect(cornerRadius: 14))
        HStack(spacing: 10) {
            Button { answer(false) } label: { Text("Not now").frame(maxWidth: .infinity) }
                .glassButton()
            Button { save() } label: {
                Group {
                    if saving { ProgressView().tint(.black) } else { Text("Save") }
                }
                .foregroundStyle(.black)
                .frame(maxWidth: .infinity)
            }
            .primaryGlass()
            .disabled(!ready)
        }
        .controlSize(.large)
        Label(
            "Kept only on your desktop. The agent uses it by name and never sees it.",
            systemImage: "lock.fill"
        )
        .font(.caption)
        .foregroundStyle(.secondary)
        if request.tainted {
            Label(
                "Asked for after the agent read a page or command output. Only go on if you asked for something that needs this key.",
                systemImage: "exclamationmark.triangle.fill"
            )
            .font(.caption)
            .foregroundStyle(.orange)
        }
    }

    private func save() {
        guard let store, let client else { return }
        saving = true
        Task {
            if await store.save(secret: value, for: approval, client: client) {
                value = ""
                // The clipboard still holds the key: clear it (writing needs no permission).
                if pasted { Clipboard.clear() }
            }
            saving = false
        }
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        Task { await store.answer(approval, approved: approved, client: client) }
    }
}

// MARK: - Action needed (Grok's login handoff)

private struct ActionCard: View {
    let action: Action
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client
    @Environment(\.takeOver) private var takeOver

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Computer", systemImage: "laptopcomputer")
                .font(.footnote.weight(.semibold))
                .foregroundStyle(.secondary)
            Text("Action needed").font(.title3.bold())
            Text(action.reason)
            if let path = action.imagePath {
                RemoteImage(path: path)
                    .onTapGesture { takeOver?.open(action) }
            }
            if let title = pageTitle {
                Text(title).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            HStack(spacing: 8) {
                Button { resolve(false) } label: { Text("Skip").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { resolve(true) } label: { Text("I'm done").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { takeOver?.open(action) } label: {
                    Text("Take over").foregroundStyle(.black).frame(maxWidth: .infinity)
                }
                .primaryGlass()
                .disabled(takeOver == nil)
            }
            .controlSize(.large)
            .lineLimit(1)
            Label {
                Text("You sign in inside your desktop's browser and the session stays there. The site may email you about a new device.")
            } icon: {
                Image(systemName: "checkmark.shield")
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .card()
    }

    private var pageTitle: String? {
        let title = action.page.components(separatedBy: " | ").first ?? ""
        return title.isEmpty ? nil : title
    }

    private func resolve(_ done: Bool) {
        guard let store, let client else { return }
        Task { await store.resolve(action, done: done, client: client) }
    }
}

// MARK: - Screenshots the agent shows

private struct PictureRow: View {
    let picture: ChatCore.Picture  // the Mac also has QuickDraw's Picture

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            RemoteImage(path: picture.path)
            if let caption = picture.caption, !caption.isEmpty {
                Text(caption).font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}
