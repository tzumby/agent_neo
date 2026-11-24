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
        case .user(let message): UserBubble(text: message.text)
        case .assistant(let message):
            if message.quiet { QuietLine(text: message.text) } else { AssistantMessage(text: message.text) }
        case .report(let report): ReportRow(report: report)
        case .approval(let approval):
            if let request = approval.secret {
                SecretRequestCard(approval: approval, request: request)
            } else if let proposal = approval.schedule {
                ScheduleApprovalCard(approval: approval, proposal: proposal)
            } else if let request = approval.access {
                AccessApprovalCard(approval: approval, request: request)
            } else if let check = approval.stuck {
                StuckCard(approval: approval, check: check)
            } else if let action = approval.browser {
                BrowserApprovalCard(approval: approval, action: action)
            } else {
                ApprovalCard(approval: approval)
            }
        case .action(let action): ActionCard(action: action)
        case .image(let picture): PictureRow(picture: picture)
        case .plan(let plan): PlanRow(plan: plan)
        }
    }
}

// MARK: - Messages

private struct UserBubble: View {
    let text: String
    @State private var selecting = false

    var body: some View {
        Text(text)
            #if os(macOS)
            .textSelection(.enabled)
            #endif
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .background(Theme.userBubble, in: .rect(cornerRadius: 22))
            .contextMenu {
                Button("Copy", systemImage: "doc.on.doc") { Clipboard.copy(text) }
                Button("Select Text", systemImage: "text.cursor") { selecting = true }
            }
            .selectTextSheet(text, isPresented: $selecting)
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
            MessageActions(text: text)
        }
    }
}

/// Under an answer: copy it, or open its text to select any part.
private struct MessageActions: View {
    let text: String
    @State private var selecting = false

    var body: some View {
        HStack(spacing: 4) {
            CopyButton(text: text)
            Button {
                selecting = true
            } label: {
                Image(systemName: "text.cursor")
                    .frame(width: 28, height: 28)
                    .contentShape(.rect)
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .accessibilityLabel("Select text")
            .help("Select text")
            .selectTextSheet(text, isPresented: $selecting)
        }
        .font(.app(.footnote))
    }
}

extension View {
    fileprivate func selectTextSheet(_ text: String, isPresented: Binding<Bool>) -> some View {
        sheet(isPresented: isPresented) { SelectableTextSheet(text: text) }
    }
}

/// A message as plain text, to select any part of it and copy that.
private struct SelectableTextSheet: View {
    let text: String
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            SelectableText(text: text)
                .navigationTitle("Select text")
                #if os(iOS)
                .navigationBarTitleDisplayMode(.inline)
                #endif
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Done") { dismiss() }
                    }
                    ToolbarItem(placement: .primaryAction) {
                        Button("Copy all", systemImage: "doc.on.doc") {
                            Clipboard.copy(text)
                            dismiss()
                        }
                    }
                }
        }
        #if os(macOS)
        .frame(minWidth: 520, minHeight: 420)
        #endif
    }
}

#if os(iOS)
/// UITextView: SwiftUI's selectable Text only offers the whole text on a phone.
private struct SelectableText: UIViewRepresentable {
    let text: String

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.isEditable = false
        view.isSelectable = true
        view.backgroundColor = .clear
        view.adjustsFontForContentSizeCategory = true
        view.font = .preferredFont(forTextStyle: .body)
        view.textColor = .label
        view.textContainerInset = UIEdgeInsets(top: 16, left: 12, bottom: 24, right: 12)
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        if view.text != text { view.text = text }
    }
}
#else
private struct SelectableText: View {
    let text: String

    var body: some View {
        ScrollView {
            Text(text)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(16)
        }
    }
}
#endif

/// A scheduled run's report, not a message from the user: the schedule and
/// when, its instructions on a tap, then what it found. A run that found
/// nothing is one grey line with its own words ("No spike: 5.4k requests"),
/// earlier such runs folded under it; a result is a card.
private struct ReportRow: View {
    let report: Report
    @State private var showInstructions = false
    @State private var showEarlier = false

    var body: some View {
        if let answer = report.answer, !report.quiet {
            VStack(alignment: .leading, spacing: 12) {
                header
                if showInstructions { instructions }
                MarkdownView(text: answer.text)
                MessageActions(text: answer.text)
            }
            .card()
        } else {
            VStack(alignment: .leading, spacing: 8) {
                header
                if showInstructions { instructions }
                if let answer = report.answer {
                    QuietLine(text: answer.text)
                    if !report.earlier.isEmpty { earlier }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// "APX attack spike watch · 12:00", or for folded runs "· 4 checks, 9:00–12:00".
    private var header: some View {
        Button {
            withAnimation(.snappy(duration: 0.2)) { showInstructions.toggle() }
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Image(systemName: "clock")
                Text(report.schedule).fontWeight(.semibold).foregroundStyle(.primary.opacity(0.85))
                Text("· \(when)")
                Image(systemName: "chevron.right")
                    .font(.app(.caption2).weight(.bold))
                    .rotationEffect(.degrees(showInstructions ? 90 : 0))
            }
            .font(.app(.footnote))
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .help("Scheduled task. Show the instructions it runs with")
        .contextMenu {
            if let answer = report.answer {
                Button("Copy the report", systemImage: "doc.on.doc") { Clipboard.copy(answer.text) }
            }
            Button("Copy the instructions", systemImage: "doc.on.doc") { Clipboard.copy(report.instructions) }
        }
    }

    private var when: String {
        guard let first = report.earlier.first else { return Self.time(report.ts) }
        return "\(report.earlier.count + 1) checks, \(Self.time(first.ts))–\(Self.time(report.ts))"
    }

    private var instructions: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Instructions").font(.app(.caption).weight(.semibold))
            Text(report.instructions).font(.app(.footnote)).textSelection(.enabled)
        }
        .foregroundStyle(.secondary)
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.inset, in: .rect(cornerRadius: 14))
        .transition(.opacity)
    }

    /// The folded runs, newest first, with their times.
    private var earlier: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button {
                withAnimation(.snappy(duration: 0.2)) { showEarlier.toggle() }
            } label: {
                HStack(spacing: 4) {
                    Text(report.earlier.count == 1 ? "1 earlier check" : "\(report.earlier.count) earlier checks")
                    Image(systemName: "chevron.right")
                        .font(.app(.caption2).weight(.bold))
                        .rotationEffect(.degrees(showEarlier ? 90 : 0))
                }
                .font(.app(.caption).weight(.medium))
                .foregroundStyle(.tertiary)
                .contentShape(.rect)
            }
            .buttonStyle(.plain)
            .padding(.leading, 26)
            if showEarlier {
                ForEach(report.earlier.reversed(), id: \.id) { run in
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Text(Self.time(run.ts)).monospacedDigit().foregroundStyle(.tertiary)
                        if let answer = run.answer { QuietLine.inline(answer.text) }
                    }
                    .font(.app(.footnote))
                    .foregroundStyle(.secondary)
                    .padding(.leading, 26)
                }
            }
        }
    }

    /// Today: "12:00"; earlier: "Nov 1, 12:00".
    static func time(_ ts: String) -> String {
        guard let date = Date(agentTimestamp: ts) else { return "" }
        return Calendar.current.isDateInToday(date)
            ? date.formatted(date: .omitted, time: .shortened)
            : date.formatted(.dateTime.month(.abbreviated).day().hour().minute())
    }
}

/// A run that found nothing: its own one line, grey. (Its phone stayed quiet.)
private struct QuietLine: View {
    let text: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: "moon.zzz").font(.app(.footnote))
            Self.inline(text).fixedSize(horizontal: false, vertical: true)
        }
        .font(.app(.subheadline))
        .foregroundStyle(.secondary)
        .contextMenu {
            Button("Copy", systemImage: "doc.on.doc") { Clipboard.copy(text) }
        }
    }

    /// The line's Markdown (bold, code, links), inline.
    static func inline(_ text: String) -> Text {
        if case .paragraph(let attributed)? = MarkdownBlock.parse(text).first {
            return MarkdownText.text(attributed)
        }
        return Text(text)
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

/// The person's own version of a card's text, while they edit it.
private struct CardDraft {
    var editing = false
    var text = ""

    /// What to send with the approval: nothing if they didn't change it.
    func changed(from original: String) -> String? {
        editing && text != original ? text : nil
    }

    /// Blank text can't be approved: Deny says no.
    var isBlank: Bool {
        editing && text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}

/// "Edit", then "Undo changes", which goes back to the agent's words.
private struct DraftToggle: View {
    @Binding var draft: CardDraft
    let original: String

    var body: some View {
        Button {
            withAnimation(.snappy(duration: 0.2)) {
                draft.text = original
                draft.editing.toggle()
            }
        } label: {
            Label(draft.editing ? "Undo changes" : "Edit", systemImage: draft.editing ? "arrow.uturn.backward" : "pencil")
                .font(.app(.footnote).weight(.medium))
                .foregroundStyle(.secondary)
                .contentShape(.rect)
        }
        .buttonStyle(.plain)
    }
}

private struct DraftField: View {
    let label: String
    @Binding var text: String
    var monospaced = false

    var body: some View {
        TextField(label, text: $text, axis: .vertical)
            .lineLimit(3...12)
            .font(monospaced ? .app(.footnote).monospaced() : .app(.body))
            .textFieldStyle(.plain)
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.inset, in: .rect(cornerRadius: 12))
    }
}

/// A command that needs the user's OK: what it does in plain words and why
/// it needs the OK; the command itself only when asked for.
private struct ApprovalCard: View {
    let approval: Approval
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client
    @State private var showCommand = false
    @State private var draft = CardDraft()

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label {
                Text("Needs your OK").font(.app(.subheadline).weight(.semibold))
            } icon: {
                Image(systemName: "exclamationmark.shield.fill")
            }
            .foregroundStyle(.orange)
            Text(approval.summary ?? "Run a command on your desktop").font(.app(.headline))
            Text("It \(approval.reason).").font(.app(.subheadline)).foregroundStyle(.secondary)
            if approval.tainted {
                Label(
                    "The agent read web pages or other outside content before asking. Check the command if this isn't what you asked for.",
                    systemImage: "exclamationmark.triangle.fill"
                )
                .font(.app(.caption))
                .foregroundStyle(.orange)
            }
            HStack {
                if !draft.editing {
                    Button {
                        withAnimation(.snappy(duration: 0.2)) { showCommand.toggle() }
                    } label: {
                        HStack(spacing: 4) {
                            Text(showCommand ? "Hide the command" : "Show the command")
                            Image(systemName: "chevron.right")
                                .font(.app(.caption2).weight(.bold))
                                .rotationEffect(.degrees(showCommand ? 90 : 0))
                        }
                        .font(.app(.footnote).weight(.medium))
                        .foregroundStyle(.secondary)
                        .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                }
                Spacer()
                if approval.editable {
                    DraftToggle(draft: $draft, original: approval.command)
                }
            }
            if draft.editing {
                DraftField(label: "The command", text: $draft.text, monospaced: true)
            } else if showCommand {
                Text(approval.command)
                    .font(.app(.footnote).monospaced())
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
                .disabled(draft.isBlank)
            }
            .controlSize(.large)
        }
        .card()
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        let text = approved ? draft.changed(from: approval.command) : nil
        Task { await store.answer(approval, approved: approved, text: text, client: client) }
    }
}

/// "Write this?": the agent wants to type, press a key or click something
/// that may send or change things in the desktop's browser.
/// A keyboard key, drawn like a keycap.
private struct KeyCap: View {
    let key: String

    var body: some View {
        Text(key)
            .font(.app(.callout).monospaced().weight(.semibold))
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
    @State private var draft = CardDraft()

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: icon).foregroundStyle(.orange)
                Text(title).font(.app(.headline))
                Spacer()
                if !action.site.isEmpty {
                    Label(site, systemImage: "globe").font(.app(.caption)).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            // A click or key says little by itself. The agent's reason first, then
            // what the page itself shows, which the agent's words can't change.
            if action.action != "type", let summary = approval.summary {
                Text("The agent says: \(summary)").font(.app(.subheadline).weight(.medium))
            }
            // What the agent would do, shown as a quote or a key, not a box that
            // looks like a text field: the card asks for a yes or a no, until
            // the person chooses to edit it.
            switch action.action {
            case "type":
                if !action.target.isEmpty {
                    Text("In “\(action.target)”").font(.app(.footnote)).foregroundStyle(.secondary).lineLimit(2)
                }
                if draft.editing {
                    DraftField(label: "What to write", text: $draft.text)
                } else {
                    HStack(alignment: .top, spacing: 10) {
                        Capsule().fill(.orange.opacity(0.6)).frame(width: 3)
                        Text(action.text ?? "")
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .fixedSize(horizontal: false, vertical: true)
                }
                if approval.editable {
                    DraftToggle(draft: $draft, original: action.text ?? "")
                }
            case "key":
                HStack(spacing: 8) {
                    KeyCap(key: action.key ?? "?")
                    if !action.target.isEmpty {
                        Text("in “\(action.target)”").foregroundStyle(.secondary).lineLimit(2)
                    }
                }
                .font(.app(.callout))
            default:
                Text(clickTarget).font(.app(.callout).weight(.medium)).lineLimit(3)
            }
            if action.action != "type" {
                pageFacts
            }
            if action.action == "type", action.submit {
                Label("Then presses Enter, which may send it", systemImage: "return")
                    .font(.app(.footnote))
                    .foregroundStyle(.orange)
            } else if action.action == "type", action.sends {
                Label("Approving also lets it send this", systemImage: "paperplane")
                    .font(.app(.footnote))
                    .foregroundStyle(.orange)
            } else if action.action != "type", !approval.reason.isEmpty {
                Text("It \(approval.reason).").font(.app(.footnote)).foregroundStyle(.secondary)
            }
            // Shown only while it waits: answered, it leaves the chat.
            HStack(spacing: 10) {
                Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { answer(true) } label: {
                    Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                }
                .primaryGlass()
                .disabled(draft.isBlank)
            }
            .controlSize(.large)
        }
        .card()
    }

    @ViewBuilder private var pageFacts: some View {
        if !action.around.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                Text("Around it on the page").font(.app(.caption).weight(.semibold))
                Text(action.around)
                    .font(.app(.footnote))
                    .lineLimit(6)
                    .textSelection(.enabled)
            }
            .foregroundStyle(.secondary)
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.inset, in: .rect(cornerRadius: 14))
        }
        if !action.page.isEmpty || !action.url.isEmpty {
            VStack(alignment: .leading, spacing: 1) {
                if !action.page.isEmpty { Text(action.page).lineLimit(1) }
                if !action.url.isEmpty { Text(action.url).lineLimit(1).truncationMode(.middle) }
            }
            .font(.app(.caption))
            .foregroundStyle(.secondary)
        }
        if let path = action.imagePath {
            RemoteImage(path: path)
        }
    }

    /// Button “Confirm”
    private var clickTarget: String {
        let name = action.target.isEmpty ? "without a name" : "“\(action.target)”"
        return action.role.isEmpty ? name : "\(action.role.prefix(1).uppercased())\(action.role.dropFirst()) \(name)"
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
        let text = approved ? draft.changed(from: action.text ?? "") : nil
        Task { await store.answer(approval, approved: approved, text: text, client: client) }
    }
}

private struct Outcome: View {
    let text: String
    let icon: String
    let color: Color

    var body: some View {
        Label(text, systemImage: icon).font(.app(.subheadline).weight(.medium)).foregroundStyle(color)
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
    @State private var draft = CardDraft()

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label {
                Text("Schedule this?").font(.app(.headline))
            } icon: {
                Image(systemName: "calendar.badge.exclamationmark").foregroundStyle(.orange)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text(proposal.name).font(.app(.subheadline).weight(.semibold))
                Text(proposal.when).font(.app(.subheadline))
                if !draft.editing {
                    Text(proposal.prompt)
                        .font(.app(.footnote))
                        .foregroundStyle(.secondary)
                        .lineLimit(8)
                        .textSelection(.enabled)
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.inset, in: .rect(cornerRadius: 14))
            if draft.editing {
                DraftField(label: "What to do each time", text: $draft.text)
            }
            if approval.editable {
                DraftToggle(draft: $draft, original: proposal.prompt)
            }
            Text("The agent asked for this after reading outside content. Approve only if you asked for it.")
                .font(.app(.footnote))
                .foregroundStyle(.secondary)
            HStack(spacing: 10) {
                Button { answer(false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { answer(true) } label: {
                    Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                }
                .primaryGlass()
                .disabled(draft.isBlank)
            }
            .controlSize(.large)
        }
        .card()
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        let text = approved ? draft.changed(from: proposal.prompt) : nil
        Task { await store.answer(approval, approved: approved, text: text, client: client) }
    }
}

/// The agent asks to reach sites the desktop's proxy refuses.
private struct AccessApprovalCard: View {
    let approval: Approval
    let request: AccessRequest
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client

    private var one: Bool { request.hosts.count == 1 }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label {
                Text(one ? "Let your agent reach this site?" : "Let your agent reach these sites?")
                    .font(.app(.headline))
            } icon: {
                Image(systemName: "globe").foregroundStyle(.orange)
            }
            VStack(alignment: .leading, spacing: 4) {
                ForEach(request.hosts, id: \.self) { host in
                    Text(host).font(.app(.subheadline).monospaced()).textSelection(.enabled)
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.inset, in: .rect(cornerRadius: 14))
            if !request.why.isEmpty {
                Text(request.why).font(.app(.subheadline))
            }
            Text("Its commands can reach \(one ? "it" : "them") from now on, scheduled checks included. You can remove sites in the web app's Settings.")
                .font(.app(.footnote))
                .foregroundStyle(.secondary)
            if approval.tainted {
                Label(
                    "Asked for after the agent read a page or command output. Allow only if you asked for something that needs \(one ? "this site" : "these sites").",
                    systemImage: "exclamationmark.triangle"
                )
                .font(.app(.footnote))
                .foregroundStyle(.orange)
            }
            HStack(spacing: 10) {
                Button { answer(false) } label: { Text("Don't allow").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { answer(true) } label: {
                    Text("Allow").foregroundStyle(.black).frame(maxWidth: .infinity)
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

// MARK: - A task's plan and progress

/// The agent's own steps for a task, ticked off as it goes; folded once the
/// task ends.
private struct PlanRow: View {
    let plan: Plan
    @State private var unfolded = false

    var body: some View {
        if plan.finished {
            VStack(alignment: .leading, spacing: 8) {
                Button {
                    withAnimation(.snappy(duration: 0.2)) { unfolded.toggle() }
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "checklist")
                        Text("Plan · \(count) done")
                        Image(systemName: "chevron.right")
                            .font(.app(.caption2).weight(.bold))
                            .rotationEffect(.degrees(unfolded ? 90 : 0))
                    }
                    .font(.app(.footnote))
                    .foregroundStyle(.secondary)
                    .contentShape(.rect)
                }
                .buttonStyle(.plain)
                if unfolded { steps.transition(.opacity) }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Label("Plan", systemImage: "checklist").font(.app(.subheadline).weight(.semibold))
                    Spacer()
                    Text(count).font(.app(.footnote)).monospacedDigit().foregroundStyle(.secondary)
                }
                steps
            }
            .card()
            .accessibilityElement(children: .contain)
            .accessibilityLabel("Plan, \(count) done")
        }
    }

    private var count: String { "\(plan.done) of \(plan.steps.count)" }

    private var steps: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(plan.steps.enumerated()), id: \.offset) { _, step in
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Image(systemName: Self.mark(step.status))
                        .foregroundStyle(step.status == .pending ? .tertiary : .secondary)
                    Text(step.step)
                        .foregroundStyle(step.status == .done ? .secondary : .primary)
                        .strikethrough(step.status == .done, color: .secondary)
                }
                .font(.app(.subheadline))
                .accessibilityElement(children: .combine)
                .accessibilityLabel("\(Self.said(step.status)): \(step.step)")
            }
        }
    }

    private static func mark(_ status: PlanStep.Status) -> String {
        switch status {
        case .done: "checkmark.circle.fill"
        case .inProgress: "circle.lefthalf.filled"
        case .pending: "circle"
        }
    }

    private static func said(_ status: PlanStep.Status) -> String {
        switch status {
        case .done: "Done"
        case .inProgress: "In progress"
        case .pending: "To do"
        }
    }
}

/// A check on the running task thinks it's going in circles: carry on, or
/// stop it (it then ends with what's done and what's left). Never answered
/// for you.
private struct StuckCard: View {
    let approval: Approval
    let check: StuckCheck
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label {
                Text("Checked on your task").font(.app(.subheadline).weight(.semibold))
            } icon: {
                Image(systemName: "clock")
            }
            .foregroundStyle(.orange)
            Text("Looks stuck").font(.app(.headline))
            if !check.reason.isEmpty {
                Text(check.reason).font(.app(.subheadline))
            }
            if !facts.isEmpty {
                Text(facts).font(.app(.footnote)).foregroundStyle(.secondary)
            }
            Text("Stop ends it with what's done and what's left. Keep going lets it carry on, with a hint.")
                .font(.app(.footnote))
                .foregroundStyle(.secondary)
            HStack(spacing: 10) {
                Button { answer(false) } label: { Text("Stop").frame(maxWidth: .infinity) }
                    .glassButton()
                Button { answer(true) } label: {
                    Text("Keep going").foregroundStyle(.black).frame(maxWidth: .infinity)
                }
                .primaryGlass()
            }
            .controlSize(.large)
        }
        .card()
    }

    /// "Working for 33 min · 70 steps · plan 2 of 5 done"
    private var facts: String {
        [
            ChatTimeline.duration(minutes: Double(check.minutes)).map { "Working for \($0)" },
            check.steps > 0 ? "\(check.steps) \(check.steps == 1 ? "step" : "steps")" : nil,
            check.plan.map { "plan \($0.done) of \($0.total) done" },
        ].compactMap { $0 }.joined(separator: " · ")
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
                Text(request.replace ? "A new \(request.label)" : request.label).font(.app(.headline))
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
                .font(.app(.subheadline))
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
        .font(.app(.caption))
        .foregroundStyle(.secondary)
        if request.tainted {
            Label(
                "Asked for after the agent read a page or command output. Only go on if you asked for something that needs this key.",
                systemImage: "exclamationmark.triangle.fill"
            )
            .font(.app(.caption))
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
                .font(.app(.footnote).weight(.semibold))
                .foregroundStyle(.secondary)
            Text("Action needed").font(.app(.title3).bold())
            Text(action.reason)
            if let path = action.imagePath {
                RemoteImage(path: path)
                    .onTapGesture { takeOver?.open(action) }
            }
            if let title = pageTitle {
                Text(title).font(.app(.caption)).foregroundStyle(.secondary).lineLimit(1)
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
            .font(.app(.caption))
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
                Text(caption).font(.app(.caption)).foregroundStyle(.secondary)
            }
        }
    }
}
