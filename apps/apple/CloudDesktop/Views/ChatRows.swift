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
        case .user(let message): UserBubble(text: message.text)
        case .assistant(let message): AssistantMessage(text: message.text)
        case .work(let work): WorkRow(work: work)
        case .terminal(let terminal): TerminalCard(terminal: terminal)
        case .approval(let approval):
            if let action = approval.browser {
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

    var body: some View {
        Text(text)
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .background(Theme.userBubble, in: .rect(cornerRadius: 22))
            .contextMenu {
                Button("Copy", systemImage: "doc.on.doc") { UIPasteboard.general.string = text }
            }
            .frame(maxWidth: .infinity, alignment: .trailing)
            .padding(.leading, 56)
    }
}

/// The agent's answer: plain text on the background, as in Grok, with code
/// blocks as cards.
private struct AssistantMessage: View {
    let text: String

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            ForEach(Array(MarkdownText.blocks(text).enumerated()), id: \.offset) { _, block in
                switch block {
                case .text(let attributed):
                    // No .textSelection here: selectable text skips custom renderers
                    // (no code chips). Long-press copies instead; code blocks stay selectable.
                    MarkdownText.text(attributed)
                        .lineSpacing(3)
                        .textRenderer(CodeChipRenderer())
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contextMenu {
                            Button("Copy", systemImage: "doc.on.doc") { UIPasteboard.general.string = text }
                        }
                case .code(let language, let code):
                    CodeBlock(language: language, code: code)
                }
            }
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

/// Markdown the way chat answers use it: fenced code blocks become blocks;
/// the rest is inline Markdown (bold, italics, links, `code` as a chip) with
/// line breaks kept, headings as bold lines and "-" bullets as "•".
enum MarkdownText {
    enum Block {
        case text(AttributedString)
        case code(language: String?, code: String)
    }

    static func blocks(_ text: String) -> [Block] {
        var blocks: [Block] = []
        var prose: [Substring] = []
        var code: [Substring]?
        var language: String?

        func flushProse() {
            let joined = prose.joined(separator: "\n").trimmingCharacters(in: .newlines)
            if !joined.isEmpty { blocks.append(.text(inline(joined))) }
            prose = []
        }
        for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("```") {
                if let lines = code {
                    blocks.append(.code(language: language, code: lines.joined(separator: "\n")))
                    code = nil
                } else {
                    flushProse()
                    let tag = trimmed.dropFirst(3).trimmingCharacters(in: .whitespaces)
                    language = tag.isEmpty ? nil : tag
                    code = []
                }
            } else if code != nil {
                code?.append(line)
            } else {
                prose.append(line)
            }
        }
        if let lines = code { blocks.append(.code(language: language, code: lines.joined(separator: "\n"))) }
        flushProse()
        return blocks
    }

    private static func inline(_ text: String) -> AttributedString {
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false).map { line -> String in
            let trimmed = line.drop { $0 == " " }
            if let hashes = trimmed.firstIndex(of: " "), trimmed[..<hashes].allSatisfy({ $0 == "#" }),
                !trimmed[..<hashes].isEmpty
            {
                return "**\(trimmed[trimmed.index(after: hashes)...])**"
            }
            if trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") {
                let indent = String(repeating: " ", count: line.count - trimmed.count)
                return indent + "• " + trimmed.dropFirst(2)
            }
            return String(line)
        }
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        return (try? AttributedString(markdown: lines.joined(separator: "\n"), options: options))
            ?? AttributedString(text)
    }

    /// The text with each `code` span as a monospaced run marked for
    /// `CodeChipRenderer`, which draws a rounded chip behind it.
    static func text(_ attributed: AttributedString) -> Text {
        var result = Text(verbatim: "")
        var prose = AttributedString()
        for run in attributed.runs {
            let piece = AttributedString(attributed[run.range])
            guard run.inlinePresentationIntent?.contains(.code) == true else {
                prose.append(piece)
                continue
            }
            // Thin spaces give the chip a little room on each side.
            var code = AttributedString("\u{2009}\(String(piece.characters))\u{2009}")
            code.font = .system(.callout, design: .monospaced)
            code.foregroundColor = Color(white: 0.95)
            result = Text("\(result)\(Text(prose))\(Text(code).customAttribute(InlineCode()))")
            prose = AttributedString()
        }
        return Text("\(result)\(Text(prose))")
    }
}

/// Marks inline code in a `Text` for `CodeChipRenderer`.
struct InlineCode: TextAttribute {}

/// Draws a rounded chip behind runs marked `InlineCode`.
struct CodeChipRenderer: TextRenderer {
    var displayPadding: EdgeInsets { EdgeInsets(top: 2, leading: 2, bottom: 2, trailing: 2) }

    func draw(layout: Text.Layout, in context: inout GraphicsContext) {
        for line in layout {
            // One code span can be several runs (the padding spaces may use
            // another font), so neighbouring code runs share one chip.
            var chips: [CGRect] = []
            var open: CGRect?
            for run in line {
                if run[InlineCode.self] != nil {
                    open = open?.union(run.typographicBounds.rect) ?? run.typographicBounds.rect
                } else if let chip = open {
                    chips.append(chip)
                    open = nil
                }
            }
            if let chip = open { chips.append(chip) }
            for chip in chips {
                context.fill(
                    Path(roundedRect: chip.insetBy(dx: 0, dy: -1.5), cornerRadius: 6),
                    with: .color(.white.opacity(0.12)))
            }
            for run in line { context.draw(run) }
        }
    }
}

/// A fenced code block: language, copy, and code that scrolls sideways.
private struct CodeBlock: View {
    let language: String?
    let code: String

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(language ?? "code").font(.caption.weight(.semibold))
                Spacer()
                CopyButton(text: code).font(.caption)
            }
            .foregroundStyle(.secondary)
            .padding(.leading, 12)
            .padding(.trailing, 4)
            .padding(.vertical, 2)
            Rectangle().fill(Theme.hairline).frame(height: 0.5)
            ScrollView(.horizontal) {
                Text(code)
                    .font(.system(.footnote, design: .monospaced))
                    .textSelection(.enabled)
                    .padding(12)
            }
            .scrollIndicators(.hidden)
        }
        .background(Theme.inset, in: .rect(cornerRadius: 16))
        .overlay(RoundedRectangle(cornerRadius: 16).strokeBorder(Theme.hairline, lineWidth: 0.5))
    }
}

// MARK: - Terminal (a shell command, its approval and its output)

/// One card per command: what ran, Approve/Deny when it needs you, and what it
/// printed, in an inset panel like the approval card's command box.
private struct TerminalCard: View {
    let terminal: Terminal
    @Environment(\.chatStore) private var store
    @Environment(\.agentClient) private var client
    @State private var showAll = false
    private let tailLines = 12

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                Image(systemName: "apple.terminal").foregroundStyle(.secondary)
                Text("Terminal").font(.headline)
                if terminal.asRoot {
                    Text("root")
                        .font(.caption.bold())
                        .foregroundStyle(.orange)
                        .padding(.horizontal, 7)
                        .padding(.vertical, 2)
                        .background(.orange.opacity(0.15), in: .capsule)
                }
                Spacer()
                status.font(.subheadline)
            }

            HStack(alignment: .firstTextBaseline, spacing: 0) {
                Text("$ ").foregroundStyle(.green)
                Text(terminal.command)
            }
            .font(.system(.callout, design: .monospaced))
            .textSelection(.enabled)

            if let approval = terminal.approval, approval.state == .pending {
                Text("It \(approval.reason). Run it?").font(.footnote).foregroundStyle(.secondary)
                HStack(spacing: 10) {
                    Button { answer(approval, approved: false) } label: { Text("Deny").frame(maxWidth: .infinity) }
                        .buttonStyle(.glass)
                    Button { answer(approval, approved: true) } label: {
                        Text("Approve").foregroundStyle(.black).frame(maxWidth: .infinity)
                    }
                    .primaryGlass()
                }
                .controlSize(.large)
            }

            if terminal.state == .finished {
                output
            } else if terminal.state == .unrecorded {
                Text("Output not recorded").font(.footnote).foregroundStyle(.tertiary)
            }
        }
        .card()
        .sensoryFeedback(.success, trigger: terminal.state) { _, new in new == .finished && terminal.exitCode == 0 }
    }

    /// What the command printed, in an inset panel; it scrolls sideways so
    /// columns stay lined up, and long output shows its last lines first.
    @ViewBuilder private var output: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("Output").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                Spacer()
                CopyButton(text: terminal.output.isEmpty ? terminal.command : terminal.output).font(.caption)
            }
            .padding(.leading, 12)
            .padding(.trailing, 4)
            .padding(.top, 2)
            if hiddenLines > 0 {
                Button("Show \(hiddenLines) earlier lines") { withAnimation(.snappy) { showAll = true } }
                    .font(.caption)
                    .buttonStyle(.plain)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 12)
            }
            ScrollView(.horizontal) {
                Text(terminal.output.isEmpty ? "(no output)" : shownOutput)
                    .font(.system(.footnote, design: .monospaced))
                    .foregroundStyle(terminal.output.isEmpty ? Color.secondary : Color(white: 0.85))
                    .textSelection(.enabled)
                    .padding(.horizontal, 12)
            }
            .scrollIndicators(.hidden)
            .padding(.bottom, 12)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.inset, in: .rect(cornerRadius: 12))
    }

    @ViewBuilder private var status: some View {
        switch terminal.state {
        case .waitingForApproval:
            Label("Needs approval", systemImage: "hand.raised.fill").foregroundStyle(.orange)
        case .running:
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("Running").foregroundStyle(.secondary)
            }
        case .finished:
            if terminal.timedOut {
                Label("Timed out", systemImage: "clock.badge.exclamationmark").foregroundStyle(.orange)
            } else if terminal.exitCode == 0 {
                Label(duration ?? "Done", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
            } else {
                Label("Exit \(terminal.exitCode.map(String.init) ?? "?")", systemImage: "xmark.circle.fill")
                    .foregroundStyle(.red)
            }
        case .denied:
            Label("Denied", systemImage: "xmark.circle.fill").foregroundStyle(.red)
        case .didNotFinish:
            Label("Stopped", systemImage: "stop.circle.fill").foregroundStyle(.secondary)
        case .unrecorded:
            if terminal.approval?.state == .approved {
                Label("Approved", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
            }
        }
    }

    private var duration: String? {
        guard let seconds = terminal.seconds else { return nil }
        return seconds < 1 ? "<1s" : String(format: "%.0fs", seconds)
    }

    private var lines: [Substring] {
        terminal.output.split(separator: "\n", omittingEmptySubsequences: false)
    }

    private var hiddenLines: Int { showAll ? 0 : max(0, lines.count - tailLines) }

    private var shownOutput: String {
        hiddenLines > 0 ? lines.suffix(tailLines).joined(separator: "\n") : terminal.output
    }

    private func answer(_ approval: Approval, approved: Bool) {
        guard let store, let client else { return }
        Task { await store.answer(approval, approved: approved, client: client) }
    }
}

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

    var body: some View {
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
            } else {
                Text(StepText.describe(step)).font(.footnote).lineLimit(2)
            }
        }
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
            Text(approval.command)
                .font(.callout.monospaced())
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.inset, in: .rect(cornerRadius: 12))
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
        case "type": action.submit ? "Write and send this?" : "Write this?"
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
