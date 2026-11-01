import Foundation

/// The agent's answers as Markdown blocks (GitHub-flavoured: headings, lists,
/// quotes, code, tables, rules), parsed by Foundation. Text inside each block
/// keeps its inline styles (bold, italics, `code`, strikethrough, links).
///
/// Chat-style line breaks: a single newline breaks the line, as people (and
/// models) mean it. Raw HTML stays literal text, and images are never loaded:
/// they show as their description, linked to the image.
public enum MarkdownBlock: Sendable, Equatable {
    case paragraph(AttributedString)
    case heading(level: Int, AttributedString)
    case code(language: String?, code: String)
    case list(ordered: Bool, items: [MarkdownListItem])
    case quote([MarkdownBlock])
    case rule
    case table(columns: [MarkdownColumn], header: [AttributedString], rows: [[AttributedString]])

    public static func parse(_ markdown: String) -> [MarkdownBlock] {
        let options = AttributedString.MarkdownParsingOptions(
            allowsExtendedAttributes: false, interpretedSyntax: .full, failurePolicy: .returnPartiallyParsedIfPossible)
        guard let document = try? AttributedString(markdown: markdown, options: options) else {
            return [.paragraph(AttributedString(markdown))]
        }
        let root = Node(kind: nil, identity: 0)
        for run in document.runs {
            // Outermost block first: the list, its item, the item's paragraph…
            let path = (run.presentationIntent?.components ?? []).reversed()
            var node = root
            for component in path {
                node = node.child(kind: component.kind, identity: component.identity)
            }
            if path.isEmpty {
                // Outside any block (raw HTML): a paragraph of its own.
                node = root.child(kind: .paragraph, identity: -1 - root.children.count)
            }
            node.text.append(inline(document[run.range], run: run))
        }
        return root.blocks()
    }

    /// One run's text, as the chat shows it.
    private static func inline(_ slice: AttributedSubstring, run: AttributedString.Runs.Run) -> AttributedString {
        var text = AttributedString(slice)
        text.presentationIntent = nil
        if run.inlinePresentationIntent?.contains(.softBreak) == true {
            text = AttributedString("\n")
        } else if let image = run.imageURL {
            text.imageURL = nil
            text.link = image
        }
        return text
    }
}

public struct MarkdownListItem: Sendable, Equatable {
    /// The item's number in an ordered list (lists can start anywhere).
    public let ordinal: Int
    /// "- [ ]" or "- [x]": a task list item, done or not.
    public let checked: Bool?
    public let blocks: [MarkdownBlock]
}

public enum MarkdownColumn: Sendable, Equatable {
    case leading, center, trailing
}

/// The block tree, as it's rebuilt from the runs.
private final class Node {
    let kind: PresentationIntent.Kind?
    let identity: Int
    var children: [Node] = []
    var text = AttributedString()

    init(kind: PresentationIntent.Kind?, identity: Int) {
        self.kind = kind
        self.identity = identity
    }

    /// The child for this block: runs come in document order, so it's the
    /// last one if it exists already.
    func child(kind: PresentationIntent.Kind, identity: Int) -> Node {
        if let last = children.last, last.identity == identity { return last }
        let node = Node(kind: kind, identity: identity)
        children.append(node)
        return node
    }

    func blocks() -> [MarkdownBlock] { children.compactMap(\.block) }

    var block: MarkdownBlock? {
        switch kind {
        case .paragraph: return .paragraph(trimmed(text))
        case .header(let level): return .heading(level: level, trimmed(text))
        case .codeBlock(let language):
            var code = String(text.characters)
            if code.hasSuffix("\n") { code.removeLast() }
            return .code(language: language.flatMap { $0.isEmpty ? nil : $0 }, code: code)
        case .orderedList, .unorderedList:
            let items = children.map { item -> MarkdownListItem in
                var ordinal = 1
                if case .listItem(let number) = item.kind { ordinal = number }
                var blocks = item.blocks()
                let checked = Self.taskMark(in: &blocks)
                return MarkdownListItem(ordinal: ordinal, checked: checked, blocks: blocks)
            }
            if case .orderedList = kind { return .list(ordered: true, items: items) }
            return .list(ordered: false, items: items)
        case .blockQuote: return .quote(blocks())
        case .thematicBreak: return .rule
        case .table(let columns):
            let alignments = columns.map { column -> MarkdownColumn in
                switch column.alignment {
                case .center: .center
                case .right: .trailing
                default: .leading
                }
            }
            let rows = children.map { row in
                // Cells by column; a short row gets empty cells.
                var cells = Array(repeating: AttributedString(), count: alignments.count)
                for cell in row.children {
                    if case .tableCell(let column) = cell.kind, cells.indices.contains(column) {
                        cells[column] = trimmed(cell.text)
                    }
                }
                return cells
            }
            let hasHeader = children.first.map { if case .tableHeaderRow = $0.kind { true } else { false } } ?? false
            return .table(
                columns: alignments, header: hasHeader ? rows.first ?? [] : [],
                rows: hasHeader ? Array(rows.dropFirst()) : rows)
        default:
            // A block Foundation adds later: its text, if any.
            let text = trimmed(text)
            return text.characters.isEmpty ? nil : .paragraph(text)
        }
    }

    /// Takes a leading "[ ] " or "[x] " off an item's first paragraph.
    private static func taskMark(in blocks: inout [MarkdownBlock]) -> Bool? {
        guard case .paragraph(let text) = blocks.first else { return nil }
        let start = String(text.characters.prefix(4))
        let checked: Bool
        switch start {
        case "[ ] ": checked = false
        case "[x] ", "[X] ": checked = true
        default: return nil
        }
        let rest = text.characters.index(text.startIndex, offsetBy: 4)
        blocks[0] = .paragraph(AttributedString(text[rest...]))
        return checked
    }
}

private func trimmed(_ text: AttributedString) -> AttributedString {
    var start = text.startIndex
    var end = text.endIndex
    while start < end, text.characters[start].isNewline { start = text.characters.index(after: start) }
    while end > start, text.characters[text.characters.index(before: end)].isNewline {
        end = text.characters.index(before: end)
    }
    return AttributedString(text[start..<end])
}
