import AgentAPI
import ChatCore
import SwiftUI
import UIKit

/// One timeline item. Its only input is the item, so SwiftUI skips rows that
/// didn't change (see the environment values in Theme.swift).
struct ChatRow: View, Equatable {
    let item: ChatItem

    var body: some View {
        switch item {
        case .user(let message): UserBubble(text: message.text, schedule: message.schedule)
        case .assistant(let message): AssistantMessage(text: message.text)
        case .work(let work): WorkRow(work: work)
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
        case .schedule(let note): ScheduleCard(note: note)
        case .memory(let note): MemoryNoteRow(note: note)
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
                    Button("Copy", systemImage: "doc.on.doc") { UIPasteboard.general.string = text }
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

/// Copies text, with a checkmark for a moment afterwards.
struct CopyButton: View {
    let text: String
    @State private var copied = false

    var body: some View {
        Button {
            UIPasteboard.general.string = text
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

// MARK: - Terminal (a shell command, its approval and its output)

/// One card per command: what ran, Approve/Deny when it needs you, and what it
/// printed, in an inset panel like the approval card's command box.
// MARK: - Work (the agent's steps)

private struct WorkRow: View {
    let work: Work
    @State private var expanded = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Button {
                withAnimation(.snappy(duration: 0.25)) { expanded.toggle() }
            } label: {
                HStack(spacing: 6) {
                    if work.finished == nil && work.waiting {
                        Text("Waiting for your approval")
                    } else if work.finished == nil {
                        Text(working).shimmering()
                    } else {
                        Text(summary)
                    }
                    Image(systemName: "chevron.right")
                        .font(.caption2.weight(.bold))
                        .rotationEffect(.degrees(expanded ? 90 : 0))
                }
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .contentShape(.rect)
            }
            .buttonStyle(.plain)

            if expanded {
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(work.steps) { StepRow(step: $0) }
                    if let footer {
                        Text(footer).font(.caption).foregroundStyle(.tertiary).padding(.top, 2)
                    }
                }
                .padding(14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.surface, in: .rect(cornerRadius: 18))
                .transition(.opacity.combined(with: .scale(scale: 0.98, anchor: .top)))
            }
        }
    }

    private var working: String {
        guard let step = work.steps.last else { return "Working" }
        return "Working · \(StepText.describe(step))"
    }

    private var summary: String {
        guard let finished = work.finished else { return "" }
        let steps = finished.steps == 1 ? "1 step" : "\(finished.steps) steps"
        let seconds = "\(Int(finished.seconds))s"
        return switch finished.status {
        case "done": "Worked for \(seconds) · \(steps)"
        case "needs_user": "Waiting for you · \(steps)"
        case "cancelled": "Stopped after \(seconds)"
        case "max_steps": "Ran out of steps after \(seconds)"
        case "stuck": "Got stuck after \(seconds)"
        case "interrupted": "Interrupted · the desktop restarted"
        default: "Something went wrong after \(seconds)"
        }
    }

    private var footer: String? {
        guard let finished = work.finished else { return nil }
        var parts: [String] = []
        if let cost = finished.cost { parts.append(String(format: "$%.4f", cost)) }
        let tokens = finished.inputTokens + finished.outputTokens
        if tokens > 0 { parts.append(tokens >= 1000 ? String(format: "%.1fk tokens", Double(tokens) / 1000) : "\(tokens) tokens") }
        if !work.model.isEmpty { parts.append(work.model.split(separator: "/").last.map(String.init) ?? work.model) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }
}

private struct StepRow: View {
    let step: Work.Step
    @State private var showOutput = false
    private let tailLines = 12

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Image(systemName: StepText.icon(step.tool))
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .frame(width: 18)
                if step.tool == "run_shell" {
                    Text(step.detail)
                        .font(.footnote.monospaced())
                        .lineLimit(3)
                    if step.asRoot {
                        Text("root")
                            .font(.caption2.bold())
                            .foregroundStyle(.orange)
                            .padding(.horizontal, 6)
                            .padding(.vertical, 2)
                            .background(.orange.opacity(0.15), in: .capsule)
                    }
                    if !step.secrets.isEmpty {
                        Image(systemName: "key.fill")
                            .font(.caption2)
                            .foregroundStyle(.yellow)
                            .accessibilityLabel("Uses \(step.secrets.joined(separator: ", "))")
                    }
                    Spacer(minLength: 4)
                    if let run = step.run {
                        RunStatus(run: run)
                        if !run.output.isEmpty {
                            Image(systemName: "chevron.down")
                                .font(.caption2.weight(.bold))
                                .foregroundStyle(.tertiary)
                                .rotationEffect(.degrees(showOutput ? 180 : 0))
                        }
                    }
                } else {
                    Text(StepText.describe(step)).font(.footnote).lineLimit(2)
                }
            }
            .contentShape(.rect)
            .onTapGesture {
                guard step.run?.output.isEmpty == false else { return }
                withAnimation(.snappy(duration: 0.2)) { showOutput.toggle() }
            }
            // A command that needed no approval keeps its output here, out of the chat.
            if showOutput, let run = step.run {
                ScrollView(.horizontal) {
                    Text(tail(run.output))
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(Color(white: 0.85))
                        .textSelection(.enabled)
                        .padding(10)
                }
                .scrollIndicators(.hidden)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.inset, in: .rect(cornerRadius: 10))
                .padding(.leading, 28)
                .transition(.opacity)
            }
        }
    }

    private func tail(_ output: String) -> String {
        let lines = output.split(separator: "\n", omittingEmptySubsequences: false)
        return lines.count > tailLines ? lines.suffix(tailLines).joined(separator: "\n") : output
    }
}

/// How a command step ended (or that it's still running).
private struct RunStatus: View {
    let run: ShellRun

    var body: some View {
        Group {
            switch run.state {
            case .waitingForApproval:
                Image(systemName: "hand.raised.fill").foregroundStyle(.orange)
            case .running:
                ProgressView().controlSize(.mini)
            case .finished where run.timedOut:
                Image(systemName: "clock.badge.exclamationmark").foregroundStyle(.orange)
            case .finished where run.exitCode == 0:
                HStack(spacing: 3) {
                    Image(systemName: "checkmark").foregroundStyle(.green)
                    if let seconds = run.seconds, seconds >= 1 {
                        Text("\(Int(seconds.rounded()))s").foregroundStyle(.secondary)
                    }
                }
            case .finished:
                Text("exit \(run.exitCode.map(String.init) ?? "?")").foregroundStyle(.red)
            case .denied:
                Text("denied").foregroundStyle(.red)
            case .didNotFinish:
                Image(systemName: "stop.circle").foregroundStyle(.secondary)
            case .unrecorded:
                EmptyView()
            }
        }
        .font(.caption2.weight(.semibold))
    }
}

/// Human wording for the agent's tool calls.
enum StepText {
    static func describe(_ step: Work.Step) -> String {
        switch step.tool {
        case "open_url": "Opening \(host(of: step.detail) ?? "a page")"
        case "read_text": "Reading the page"
        case "snapshot": "Looking at the page"
        case "click": "Clicking"
        case "type_text": "Typing"
        case "press_key": "Pressing a key"
        case "list_tabs": "Checking the open tabs"
        case "switch_tab": "Switching tabs"
        case "show_screenshot": "Taking a screenshot"
        case "wait_for": argument("text", in: step.detail).map { "Waiting for “\($0)”" } ?? "Waiting for the page"
        case "needs_user": "Asking you to step in"
        case "run_shell": "Running \(step.detail)"
        case "remember": argument("text", in: step.detail).map { "Remembering “\($0)”" } ?? "Remembering"
        case "forget": "Forgetting something"
        case "read_skill": argument("name", in: step.detail).map { "Reading the \($0) how-to" } ?? "Reading a how-to"
        case "schedule_task": argument("name", in: step.detail).map { "Scheduling “\($0)”" } ?? "Scheduling a task"
        case "list_schedules": "Checking the schedules"
        case "cancel_schedule": "Cancelling a schedule"
        case "request_secret": argument("label", in: step.detail).map { "Asking for your \($0)" } ?? "Asking for a key"
        default: step.tool
        }
    }

    static func icon(_ tool: String) -> String {
        switch tool {
        case "run_shell": "terminal"
        case "open_url": "globe"
        case "read_text": "doc.text"
        case "snapshot": "eye"
        case "click": "cursorarrow.click"
        case "type_text", "press_key": "keyboard"
        case "list_tabs", "switch_tab": "square.on.square"
        case "show_screenshot": "camera"
        case "wait_for": "hourglass"
        case "needs_user": "hand.raised"
        case "remember", "forget": "brain"
        case "read_skill": "book"
        case "schedule_task", "list_schedules", "cancel_schedule": "calendar.badge.clock"
        case "request_secret": "key"
        default: "gearshape"
        }
    }

    private static func host(of arguments: String) -> String? {
        guard let url = argument("url", in: arguments).flatMap(URL.init(string:)), let host = url.host() else {
            return nil
        }
        return host.hasPrefix("www.") ? String(host.dropFirst(4)) : host
    }

    /// A string argument from a step's JSON arguments.
    private static func argument(_ key: String, in arguments: String) -> String? {
        guard let data = arguments.data(using: .utf8),
            let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let value = object[key] as? String, !value.isEmpty
        else { return nil }
        return value
    }
}

/// A light sweep across text while the agent works.
private struct Shimmer: ViewModifier {
    @State private var phase: CGFloat = -0.6

    func body(content: Content) -> some View {
        content
            .overlay {
                GeometryReader { geometry in
                    LinearGradient(
                        colors: [.clear, .white.opacity(0.9), .clear], startPoint: .leading, endPoint: .trailing
                    )
                    .frame(width: geometry.size.width * 0.5)
                    .offset(x: phase * geometry.size.width)
                }
                .mask(content)
                .allowsHitTesting(false)
            }
            .onAppear {
                withAnimation(.linear(duration: 1.3).repeatForever(autoreverses: false)) { phase = 1.1 }
            }
    }
}

extension View {
    fileprivate func shimmering() -> some View { modifier(Shimmer()) }
}

// MARK: - Approval

private struct ApprovalCard: View {
    let approval: Approval
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label {
                Text("Run this command?").font(.headline)
            } icon: {
                Image(systemName: "exclamationmark.shield.fill").foregroundStyle(.orange)
            }
            HStack(alignment: .firstTextBaseline, spacing: 0) {
                Text("$ ").foregroundStyle(.green)
                Text(approval.command).textSelection(.enabled)
            }
            .font(.callout.monospaced())
            .lineLimit(6)
            Text("It \(approval.reason).").font(.footnote).foregroundStyle(.secondary)
            switch approval.state {
            case .pending:
                HStack(spacing: 10) {
                    Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                        .buttonStyle(.glass)
                    Button { answer(true) } label: {
                        Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                    }
                    .primaryGlass()
                }
                .controlSize(.large)
            case .approved: Outcome(text: "Approved", icon: "checkmark.circle.fill", color: .green)
            case .denied: Outcome(text: "Denied", icon: "xmark.circle.fill", color: .red)
            case .expired: Outcome(text: "Expired", icon: "clock", color: .secondary)
            }
        }
        .card()
        .sensoryFeedback(.success, trigger: approval.state) { _, new in new == .approved }
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
            switch approval.state {
            case .pending:
                HStack(spacing: 10) {
                    Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                        .buttonStyle(.glass)
                    Button { answer(true) } label: {
                        Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                    }
                    .primaryGlass()
                }
                .controlSize(.large)
            case .approved: Outcome(text: "Approved", icon: "checkmark.circle.fill", color: .green)
            case .denied: Outcome(text: "Denied", icon: "xmark.circle.fill", color: .red)
            case .expired: Outcome(text: "Expired", icon: "clock", color: .secondary)
            }
        }
        .card()
        .sensoryFeedback(.success, trigger: approval.state) { _, new in new == .approved }
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

/// "Scheduled": a task the agent set up to run later or on a repeat.
private struct ScheduleCard: View {
    let note: ScheduleNote

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label {
                Text(note.cancelled ? "Schedule cancelled" : "Scheduled").font(.subheadline.weight(.semibold))
            } icon: {
                Image(systemName: note.cancelled ? "calendar.badge.minus" : "calendar.badge.clock")
                    .foregroundStyle(note.cancelled ? Color.secondary : Color.blue)
            }
            Text(note.name).font(.headline)
            Text(note.when).font(.subheadline)
            if !note.cancelled, let next = note.nextRun {
                Text("Next: \(next.formatted(.dateTime.weekday().month().day().hour().minute()))")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
            Text(note.prompt).font(.footnote).foregroundStyle(.secondary).lineLimit(3)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .opacity(note.cancelled ? 0.6 : 1)
        .card()
    }
}

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
            if approval.state == .pending {
                HStack(spacing: 10) {
                    Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                        .buttonStyle(.glass)
                    Button { answer(true) } label: {
                        Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                    }
                    .primaryGlass()
                }
                .controlSize(.large)
            }
        }
        .card()
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        Task { await store.answer(approval, approved: approved, client: client) }
    }
}

// MARK: - Memory

/// One line when the agent saves something to its memory ("Remembered"), or
/// proposes a rule for approval. The full list is in Settings → Memory.
private struct MemoryNoteRow: View {
    let note: MemoryNote

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: icon)
                .font(.footnote)
                .foregroundStyle(note.pending && note.state == .saved ? Color.orange : Color.secondary)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.footnote.weight(.semibold)).foregroundStyle(.secondary)
                Text(note.text)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .strikethrough(note.state == .forgotten || note.state == .rejected)
                    .lineLimit(3)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .opacity(note.state == .forgotten || note.state == .rejected ? 0.6 : 1)
    }

    private var kind: String {
        switch note.kind {
        case "rule": "rule"
        case "preference": "preference"
        default: "fact"
        }
    }

    private var icon: String {
        switch note.state {
        case .approved: "checkmark.circle"
        case .rejected, .forgotten: "xmark.circle"
        case .saved: note.pending ? "exclamationmark.bubble" : "brain"
        }
    }

    private var title: String {
        switch note.state {
        case .approved: "Approved \(kind)"
        case .rejected: "Rejected \(kind)"
        case .forgotten: "Forgotten"
        case .saved where note.pending && note.reason == "conflict":
            "Proposed correction · review it in Settings → Memory"
        case .saved where note.pending: "Proposed \(kind) · review it in Settings → Memory"
        case .saved where note.inferred: "Remembered as a hint"
        case .saved: note.kind == "fact" ? "Remembered" : "Remembered \(kind)"
        }
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
            switch approval.state {
            case .pending where request.saved:
                // Saved while the desktop was busy: carry on when it's free.
                HStack {
                    Outcome(text: "Saved on your desktop", icon: "lock.fill", color: .green)
                    Spacer()
                    Button("Continue") { answer(true) }.buttonStyle(.glass)
                }
            case .pending:
                form
            case .approved: Outcome(text: "Saved on your desktop", icon: "lock.fill", color: .green)
            case .denied: Outcome(text: "Not given", icon: "xmark.circle.fill", color: .secondary)
            case .expired: Outcome(text: "Expired", icon: "clock", color: .secondary)
            }
        }
        .card()
        .sensoryFeedback(.success, trigger: approval.state) { _, new in new == .approved }
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
            .buttonStyle(.glass)
            .controlSize(.large)
        }
        KeyField(prompt: "Paste your \(request.label)", value: $value, pasted: $pasted)
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            .background(Theme.inset, in: .rect(cornerRadius: 14))
        HStack(spacing: 10) {
            Button { answer(false) } label: { Text("Not now").frame(maxWidth: .infinity) }
                .buttonStyle(.glass)
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
                if pasted { UIPasteboard.general.items = [] }
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
                    .onTapGesture { if action.state == .pending { takeOver?.open(action) } }
            }
            if let title = pageTitle {
                Text(title).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            switch action.state {
            case .pending:
                HStack(spacing: 8) {
                    Button { resolve(false) } label: { Text("Skip").frame(maxWidth: .infinity) }
                        .buttonStyle(.glass)
                    Button { resolve(true) } label: { Text("I'm done").frame(maxWidth: .infinity) }
                        .buttonStyle(.glass)
                    Button { takeOver?.open(action) } label: {
                        Text("Take over").foregroundStyle(.black).frame(maxWidth: .infinity)
                    }
                    .primaryGlass()
                    .disabled(takeOver == nil)
                }
                .controlSize(.large)
                .lineLimit(1)
            case .done: Outcome(text: "Done", icon: "checkmark.circle.fill", color: .green)
            case .skipped: Outcome(text: "Skipped", icon: "forward.fill", color: .secondary)
            case .expired: Outcome(text: "Expired", icon: "clock", color: .secondary)
            }
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
    let picture: Picture

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            RemoteImage(path: picture.path)
            if let caption = picture.caption, !caption.isEmpty {
                Text(caption).font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}
