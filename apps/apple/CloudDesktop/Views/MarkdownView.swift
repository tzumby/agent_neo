import ChatCore
import SwiftUI

/// The agent's answer as Markdown: headings, lists (nested, numbered, tasks),
/// quotes, code blocks, tables and rules, with inline styles and links.
struct MarkdownView: View {
    let text: String

    var body: some View {
        MarkdownBlocks(blocks: MarkdownBlock.parse(text), source: text, depth: 0, spacing: 12)
    }
}

/// A run of blocks, one under the other. `source` is the whole answer, for Copy.
private struct MarkdownBlocks: View {
    let blocks: [MarkdownBlock]
    let source: String
    let depth: Int
    let spacing: CGFloat

    var body: some View {
        VStack(alignment: .leading, spacing: spacing) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                BlockView(block: block, source: source, depth: depth)
            }
        }
    }
}

private struct BlockView: View {
    let block: MarkdownBlock
    let source: String
    let depth: Int

    var body: some View {
        switch block {
        case .paragraph(let text):
            Prose(text: text, source: source)
        case .heading(let level, let text):
            Prose(text: text, source: source)
                .font(Self.font(heading: level))
                .padding(.top, level <= 2 ? 6 : 2)
        case .code(let language, let code):
            CodeBlock(language: language, code: code)
        case .list(let ordered, let items):
            ListBlock(ordered: ordered, items: items, source: source, depth: depth)
        case .quote(let blocks):
            MarkdownBlocks(blocks: blocks, source: source, depth: depth, spacing: 8)
                .foregroundStyle(.secondary)
                .padding(.leading, 14)
                .overlay(alignment: .leading) {
                    RoundedRectangle(cornerRadius: 1.5).fill(.white.opacity(0.22)).frame(width: 3)
                }
        case .rule:
            Rectangle().fill(.white.opacity(0.15)).frame(height: 1).padding(.vertical, 4)
        case .table(let columns, let header, let rows):
            TableBlock(columns: columns, header: header, rows: rows)
        }
    }

    private static func font(heading level: Int) -> Font {
        switch level {
        case 1: .app(.title2).bold()
        case 2: .app(.title3).bold()
        case 3: .app(.headline)
        default: .app(.subheadline).weight(.semibold)
        }
    }
}

/// Inline text: styles, links, and `code` as chips. Selectable on the Mac, where
/// dragging over text is what people try, though selectable text skips custom
/// renderers (no chip behind inline code). On the phone it would only ever select
/// a whole paragraph, so chips stay and the answer's Select button opens its text.
private struct Prose: View {
    let text: AttributedString
    let source: String

    var body: some View {
        MarkdownText.text(text)
            .lineSpacing(3)
            .textRenderer(CodeChipRenderer())
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            #if os(macOS)
            .textSelection(.enabled)
            #endif
            .contextMenu {
                Button("Copy", systemImage: "doc.on.doc") { Clipboard.copy(source) }
            }
    }
}

private struct ListBlock: View {
    let ordered: Bool
    let items: [MarkdownListItem]
    let source: String
    let depth: Int

    /// Room for the widest number, so the items' text lines up.
    private var numberWidth: CGFloat {
        let digits = String(items.map(\.ordinal).max() ?? 1).count
        return CGFloat(digits) * 10 + 6
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    marker(item)
                    MarkdownBlocks(blocks: item.blocks, source: source, depth: depth + 1, spacing: 6)
                }
            }
        }
    }

    @ViewBuilder private func marker(_ item: MarkdownListItem) -> some View {
        if let checked = item.checked {
            Image(systemName: checked ? "checkmark.square.fill" : "square")
                .foregroundStyle(checked ? Color.green : Color.secondary)
        } else if ordered {
            Text("\(item.ordinal).")
                .monospacedDigit()
                .foregroundStyle(.secondary)
                .frame(minWidth: numberWidth, alignment: .trailing)
        } else {
            Text(["•", "◦", "▪"][min(depth, 2)])
                .foregroundStyle(.secondary)
                .frame(minWidth: 12)
        }
    }
}

/// A table that scrolls sideways when it's wider than the screen.
private struct TableBlock: View {
    let columns: [MarkdownColumn]
    let header: [AttributedString]
    let rows: [[AttributedString]]

    var body: some View {
        ScrollView(.horizontal) {
            Grid(alignment: .leading, horizontalSpacing: 0, verticalSpacing: 0) {
                if !header.isEmpty {
                    GridRow {
                        ForEach(columns.indices, id: \.self) { column in
                            cell(header[column], column).font(.app(.subheadline).weight(.semibold))
                        }
                    }
                    line
                }
                ForEach(rows.indices, id: \.self) { row in
                    GridRow {
                        ForEach(columns.indices, id: \.self) { column in
                            cell(rows[row][column], column).font(.app(.subheadline))
                        }
                    }
                    if row < rows.count - 1 { line }
                }
            }
        }
        .scrollIndicators(.hidden)
        .background(Theme.inset, in: .rect(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Theme.hairline, lineWidth: 0.5))
    }

    private var line: some View {
        Rectangle().fill(Theme.hairline).frame(height: 0.5).gridCellUnsizedAxes(.horizontal)
    }

    /// Columns with long text get a fixed width and wrap, rather than make the
    /// table endless. (In a sideways scroll view, cells are measured without a
    /// width, so a maximum width alone doesn't make them wrap.)
    private static let wrapAfter = 32
    private static let wrappedWidth: CGFloat = 220

    private func wraps(_ column: Int) -> Bool {
        ([header] + rows).contains { row in
            row.indices.contains(column) && row[column].characters.count > Self.wrapAfter
        }
    }

    @ViewBuilder private func cell(_ text: AttributedString, _ column: Int) -> some View {
        let alignment = Self.alignment(columns[column])
        let content = MarkdownText.text(text)
            .textRenderer(CodeChipRenderer())
            .multilineTextAlignment(alignment == .trailing ? .trailing : alignment == .center ? .center : .leading)
        Group {
            if wraps(column) {
                content
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(width: Self.wrappedWidth, alignment: Alignment(horizontal: alignment, vertical: .top))
            } else {
                content.fixedSize()
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .gridColumnAlignment(alignment)
    }

    private static func alignment(_ column: MarkdownColumn) -> HorizontalAlignment {
        switch column {
        case .leading: .leading
        case .center: .center
        case .trailing: .trailing
        }
    }
}

/// Inline Markdown in a `Text`, with each `code` span marked for
/// `CodeChipRenderer`, which draws a rounded chip behind it.
enum MarkdownText {
    static func text(_ attributed: AttributedString) -> Text {
        var result = Text(verbatim: "")
        var prose = AttributedString()
        for run in attributed.runs {
            let piece = AttributedString(attributed[run.range])
            guard run.inlinePresentationIntent?.contains(.code) == true else {
                prose.append(piece)
                continue
            }
            // Narrow no-break spaces give the chip a little room on each side,
            // and keep that room on the code's line when it wraps.
            var code = AttributedString("\u{202F}\(String(piece.characters))\u{202F}")
            code.font = .app(.callout, design: .monospaced)
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
struct CodeBlock: View {
    let language: String?
    let code: String

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(language ?? "code").font(.app(.caption).weight(.semibold))
                Spacer()
                CopyButton(text: code).font(.app(.caption))
            }
            .foregroundStyle(.secondary)
            .padding(.leading, 12)
            .padding(.trailing, 4)
            .padding(.vertical, 2)
            Rectangle().fill(Theme.hairline).frame(height: 0.5)
            ScrollView(.horizontal) {
                Text(code)
                    .font(.app(.footnote, design: .monospaced))
                    .textSelection(.enabled)
                    .padding(12)
            }
            .scrollIndicators(.hidden)
        }
        .background(Theme.inset, in: .rect(cornerRadius: 16))
        .overlay(RoundedRectangle(cornerRadius: 16).strokeBorder(Theme.hairline, lineWidth: 0.5))
    }
}
