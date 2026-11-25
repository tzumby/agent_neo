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
            } else if let action = approval.app {
                AppApprovalCard(approval: approval, action: action)
            } else if let install = approval.install {
                InstallApprovalCard(approval: approval, install: install)
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

/// A command that needs the user's OK, with its arguments and access visible.
private struct ApprovalCard: View {
    let approval: Approval
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client
    @State private var showCommand = true
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
            if approval.directInternet {
                Label("Direct internet access outside the proxy for this command.", systemImage: "network")
                    .font(.app(.caption))
                    .foregroundStyle(.orange)
            }
            if !approval.savedKeys.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Saved keys this command can use").font(.app(.caption).weight(.semibold))
                    Text(approval.savedKeys.joined(separator: ", "))
                        .font(.app(.footnote).monospaced())
                        .textSelection(.enabled)
                }
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
                    .fixedSize(horizontal: false, vertical: true)
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
                    Text("In “\(action.target)”").font(.app(.footnote)).foregroundStyle(.secondary)
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
                        Text("in “\(action.target)”").foregroundStyle(.secondary)
                    }
                }
                .font(.app(.callout))
            default:
                Text(clickTarget(role: action.role, name: action.target)).font(.app(.callout).weight(.medium))
            }
            pageFacts
            if legacySend {
                Label("This older request bundled typing and sending. Deny it and ask again after updating the desktop.", systemImage: "exclamationmark.triangle")
                    .font(.app(.footnote))
                    .foregroundStyle(.orange)
            } else if action.action == "type", action.submit {
                Label("Then presses Enter, which may send it", systemImage: "return")
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
                .disabled(draft.isBlank || legacySend)
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
                    .textSelection(.enabled)
            }
            .foregroundStyle(.secondary)
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.inset, in: .rect(cornerRadius: 14))
        }
        let url = action.url.isEmpty ? action.site : action.url
        if !action.page.isEmpty || !url.isEmpty || !action.destination.isEmpty {
            VStack(alignment: .leading, spacing: 1) {
                if !action.page.isEmpty { Text(action.page) }
                if !url.isEmpty {
                    Text(url)
                        .fixedSize(horizontal: false, vertical: true)
                        .textSelection(.enabled)
                }
                if !action.destination.isEmpty {
                    Text("Destination: \(action.destination)")
                        .fixedSize(horizontal: false, vertical: true)
                        .textSelection(.enabled)
                }
            }
            .font(.app(.caption))
            .foregroundStyle(.secondary)
        }
        if let path = action.imagePath {
            RemoteImage(path: path)
        }
    }

    private var title: String {
        switch action.action {
        case "type": action.submit ? "Write this and press Enter?" : "Write this?"
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

    private var legacySend: Bool {
        action.action == "type" && action.sends && !action.submit
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        let text = approved ? draft.changed(from: action.text ?? "") : nil
        Task { await store.answer(approval, approved: approved, text: text, client: client) }
    }
}

/// "Click this in Mousepad?": the browser card's counterpart for the desktop's
/// own apps, which the agent drives through their accessibility tree, or
/// (with a model that sees the screen) by pixels of a screenshot. A batch
/// lists every step, in order. Never editable.
private struct AppApprovalCard: View {
    let approval: Approval
    let action: AppAction
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: icon).foregroundStyle(.orange)
                Text(title).font(.app(.headline))
                Spacer()
                if !action.app.isEmpty {
                    Label(action.app, systemImage: "macwindow")
                        .font(.app(.footnote))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            // The agent's reason first, then what the app itself shows, which the
            // agent's words can't change.
            if action.listsSteps || action.action != "type", let summary = approval.summary {
                Text("The agent says: \(summary)").font(.app(.subheadline).weight(.medium))
            }
            if action.listsSteps {
                AppStepList(action: action)
            } else {
                what
            }
            appFacts
            if !approval.reason.isEmpty {
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
            }
            .controlSize(.large)
        }
        .card()
    }

    /// One input: what it does to what.
    @ViewBuilder private var what: some View {
        switch action.action {
        case "type":
            if !action.target.isEmpty {
                Text("In “\(action.target)”").font(.app(.footnote)).foregroundStyle(.secondary)
            }
            AppQuote(text: action.text ?? "")
        case "key":
            HStack(spacing: 8) {
                KeyCap(key: action.key ?? "?")
                if !action.target.isEmpty {
                    Text("in “\(action.target)”").foregroundStyle(.secondary)
                }
            }
            .font(.app(.callout))
        case "set_value", "select", "scroll", "activate", "close":
            AppStepLines(words: oneStep, emphasized: true)
        default:
            let pointer = action.steps.first?.pointer == true
            Text(clickTarget(role: action.role, name: action.target) + (pointer ? " with the mouse" : ""))
                .font(.app(.callout).weight(.medium))
        }
    }

    /// Its one step in words. Where it is in the app follows, as for a click.
    private var oneStep: AppStep.Words {
        var words = (action.steps.first ?? cardStep).words(window: action.window)
        words.note = nil
        return words
    }

    /// The card's own fields as a step, for a card without steps.
    private var cardStep: AppStep {
        var step = AppStep(action: action.action)
        step.target = action.target
        step.role = action.role
        step.around = action.around
        step.text = action.text
        step.key = action.key
        step.window = action.window
        return step
    }

    @ViewBuilder private var appFacts: some View {
        if !action.listsSteps, !action.around.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                Text("Where it is in the app").font(.app(.caption).weight(.semibold))
                Text(action.around)
                    .font(.app(.footnote))
                    .textSelection(.enabled)
            }
            .foregroundStyle(.secondary)
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.inset, in: .rect(cornerRadius: 14))
        }
        if !action.window.isEmpty {
            Text(action.window)
                .font(.app(.caption))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
        }
        if let path = action.imagePath {
            VStack(alignment: .leading, spacing: 6) {
                if let frame = action.frame, !action.marks.isEmpty {
                    MarkedScreenshot(path: path, frame: frame, marks: action.marks)
                    Text("The desktop when the agent asked, numbered by step")
                        .font(.app(.caption))
                        .foregroundStyle(.secondary)
                } else {
                    RemoteImage(path: path)
                    Text("The desktop when the agent asked").font(.app(.caption)).foregroundStyle(.secondary)
                }
            }
        }
    }

    private var title: String {
        let app = action.app.isEmpty ? "the app" : action.app
        return switch action.action {
        case "type": "Write this in \(app)?"
        case "key": "Press \(action.key ?? "a key") in \(app)?"
        case "set_value": "Set this in \(app)?"
        case "select": "Select this in \(app)?"
        case "scroll": "Scroll in \(app)?"
        case "activate": "Switch windows in \(app)?"
        case "close": "Close this window in \(app)?"
        case "act", "gui": "Do this in \(app)?"
        default: "Click this in \(app)?"
        }
    }

    private var icon: String {
        switch action.action {
        case "type", "set_value": "keyboard"
        case "key": action.key == "Enter" ? "return" : "keyboard"
        case "activate", "close": "macwindow"
        case "act": "list.number"
        default: "cursorarrow.click.2"
        }
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        Task { await store.answer(approval, approved: approved, text: nil, client: client) }
    }
}

/// An app card's steps, numbered. A pixel step's number is orange, as its
/// mark on the picture is.
private struct AppStepList: View {
    let action: AppAction

    var body: some View {
        let marked = Set(action.marks.map(\.step))
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(action.steps.enumerated()), id: \.offset) { index, step in
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    StepNumber(number: index + 1, marked: marked.contains(index + 1))
                    AppStepLines(words: step.words(pixels: action.action == "gui", window: action.window))
                }
            }
        }
    }
}

private struct StepNumber: View {
    let number: Int
    let marked: Bool

    var body: some View {
        Text("\(number)")
            .font(.app(.caption2).weight(.bold).monospacedDigit())
            .foregroundStyle(marked ? AnyShapeStyle(.black) : AnyShapeStyle(.secondary))
            .frame(minWidth: 20, minHeight: 20)
            .background {
                if marked {
                    Capsule().fill(.orange)
                } else {
                    Capsule().strokeBorder(.secondary.opacity(0.5), lineWidth: 1)
                }
            }
    }
}

/// A step in plain words: what it does, a keycap, the full text it writes,
/// and, muted, where it is in the app and a window other than the card's.
private struct AppStepLines: View {
    let words: AppStep.Words
    var emphasized = false

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(words.text)
                if let key = words.key {
                    KeyCap(key: key)
                }
            }
            .font(emphasized ? .app(.callout).weight(.medium) : .app(.callout))
            .foregroundStyle(words.quiet ? HierarchicalShapeStyle.secondary : .primary)
            .fixedSize(horizontal: false, vertical: true)
            if let note = words.note {
                Text(note).font(.app(.caption)).foregroundStyle(.secondary).textSelection(.enabled)
            }
            if let quote = words.quote {
                AppQuote(text: quote)
            }
            if let window = words.window {
                Text(window).font(.app(.caption)).foregroundStyle(.secondary).textSelection(.enabled)
            }
        }
    }
}

/// What it writes, in full, its lines kept.
private struct AppQuote: View {
    let text: String

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Capsule().fill(.orange.opacity(0.6)).frame(width: 3)
            Text(text)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .fixedSize(horizontal: false, vertical: true)
    }
}

/// The screenshot a pixel batch was planned on: a ring where each step acts,
/// which leaves what it acts on in sight, with the step's number beside it,
/// and a drag's path drawn. The marks are placed in fractions of the picture,
/// so they stay put as it scales.
private struct MarkedScreenshot: View {
    let path: String
    let frame: AppAction.Frame
    let marks: [AppAction.Mark]

    var body: some View {
        RemoteImage(path: path, aspectRatio: frame.width / frame.height)
            .overlay {
                GeometryReader { geometry in
                    let size = geometry.size
                    ZStack(alignment: .topLeading) {
                        ForEach(marks, id: \.step) { mark in
                            let points = mark.points.map { CGPoint(x: $0.x * size.width, y: $0.y * size.height) }
                            if points.count > 1, let end = points.last {
                                Path { $0.addLines(points) }
                                    .stroke(.orange, style: StrokeStyle(lineWidth: 2.5, lineCap: .round, dash: [6, 4]))
                                ring(12).position(end)
                            }
                            if let start = points.first {
                                ring(20).position(start)
                                badge(mark.step).position(x: start.x + 14, y: start.y - 14)
                            }
                        }
                    }
                }
                .accessibilityHidden(true)
            }
    }

    private func ring(_ size: CGFloat) -> some View {
        ZStack {
            Circle().stroke(.white, lineWidth: 5)
            Circle().stroke(.orange, lineWidth: 2)
        }
        .frame(width: size, height: size)
    }

    private func badge(_ number: Int) -> some View {
        Text("\(number)")
            .font(.system(size: 11, weight: .bold).monospacedDigit())
            .foregroundStyle(.black)
            .padding(.horizontal, 4)
            .frame(minWidth: 18, minHeight: 18)
            .background(.orange, in: .capsule)
            .overlay(Capsule().stroke(.white, lineWidth: 1.5))
            .shadow(color: .black.opacity(0.4), radius: 2, y: 1)
    }
}

/// "Install Slack?": an app from a store (the Snap Store), installed on the
/// desktop as root. The agent's reason first, then what the store says about
/// it, which the agent's words can't change. Never editable.
private struct InstallApprovalCard: View {
    let approval: Approval
    let install: AppInstall
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: "arrow.down.app").foregroundStyle(.orange)
                Text(title).font(.app(.headline))
                Spacer()
                if !install.source.isEmpty {
                    Label(install.source, systemImage: "shippingbox")
                        .font(.app(.footnote))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            if !install.why.isEmpty {
                Text("The agent says: \(install.why)").font(.app(.subheadline).weight(.medium))
            }
            storeFacts
            if install.confinement == "strict" {
                Text("Strictly confined: it runs in the Snap sandbox with a snap's usual access, which includes your desktop user's files.")
                    .font(.app(.footnote))
                    .foregroundStyle(.secondary)
            }
            if approval.tainted {
                Label(
                    "The agent read web pages or other outside content before asking. Check this is an app you asked for.",
                    systemImage: "exclamationmark.triangle"
                )
                .font(.app(.footnote))
                .foregroundStyle(.orange)
            }
            if !approval.reason.isEmpty {
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
            }
            .controlSize(.large)
        }
        .card()
    }

    private var storeFacts: some View {
        VStack(alignment: .leading, spacing: 4) {
            if !install.publisher.isEmpty {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(install.publisher).font(.app(.subheadline).weight(.semibold))
                    if install.verified {
                        Label("Verified publisher", systemImage: "checkmark.seal.fill")
                            .font(.app(.caption).weight(.medium))
                            .foregroundStyle(.green)
                    }
                }
            }
            if !install.details.isEmpty {
                Text(install.details).font(.app(.subheadline))
            }
            if !install.summary.isEmpty {
                Text(install.summary).font(.app(.footnote)).foregroundStyle(.secondary)
            }
            if !install.name.isEmpty {
                Text(install.name)
                    .font(.app(.footnote).monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.inset, in: .rect(cornerRadius: 14))
    }

    private var title: String {
        let app = !install.app.isEmpty ? install.app : !install.name.isEmpty ? install.name : "this app"
        return "Install \(app)?"
    }

    private func answer(_ approved: Bool) {
        guard let store, let client else { return }
        Task { await store.answer(approval, approved: approved, text: nil, client: client) }
    }
}

/// What a click card calls its element: Button “Confirm”; “without a name”
/// when it has none.
private func clickTarget(role: String, name: String) -> String {
    AppStep.clickTarget(role: role, name: name)
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
