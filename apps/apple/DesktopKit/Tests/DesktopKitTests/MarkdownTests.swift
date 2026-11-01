import Foundation
import Testing

@testable import ChatCore

private func plain(_ text: AttributedString) -> String { String(text.characters) }

@Test func parsesHeadingsParagraphsAndChatLineBreaks() {
    let blocks = MarkdownBlock.parse("# Title\nFirst line\nsecond line\n\nNext paragraph")
    #expect(blocks.count == 3)
    guard case .heading(let level, let title) = blocks[0] else { Issue.record("no heading"); return }
    #expect(level == 1)
    #expect(plain(title) == "Title")
    // A single newline breaks the line, as in a chat.
    guard case .paragraph(let first) = blocks[1] else { Issue.record("no paragraph"); return }
    #expect(plain(first) == "First line\nsecond line")
    guard case .paragraph(let next) = blocks[2] else { Issue.record("no paragraph"); return }
    #expect(plain(next) == "Next paragraph")
}

@Test func keepsInlineStylesAndLinks() throws {
    let blocks = MarkdownBlock.parse("Some **bold**, `code` and [a link](https://example.com).")
    guard case .paragraph(let text) = try #require(blocks.first) else { Issue.record("no paragraph"); return }
    #expect(plain(text) == "Some bold, code and a link.")
    let bold = text.runs.first { $0.inlinePresentationIntent?.contains(.stronglyEmphasized) == true }
    #expect(bold.map { plain(AttributedString(text[$0.range])) } == "bold")
    #expect(text.runs.contains { $0.inlinePresentationIntent?.contains(.code) == true })
    #expect(text.runs.contains { $0.link == URL(string: "https://example.com") })
    // No block styling leaks into the inline text.
    #expect(text.runs.allSatisfy { $0.presentationIntent == nil })
}

@Test func parsesNestedListsWithNumbersAndTasks() throws {
    let markdown = """
        3. Third
        4. Fourth
           - nested
           - [x] done
           - [ ] to do
        """
    let blocks = MarkdownBlock.parse(markdown)
    guard case .list(let ordered, let items) = try #require(blocks.first) else { Issue.record("no list"); return }
    #expect(ordered)
    #expect(items.map(\.ordinal) == [3, 4])
    guard case .list(false, let nested) = items[1].blocks.last else { Issue.record("no nested list"); return }
    #expect(nested.map(\.checked) == [nil, true, false])
    guard case .paragraph(let done) = nested[1].blocks.first else { Issue.record("no item text"); return }
    #expect(plain(done) == "done")
}

@Test func parsesCodeQuotesRulesAndTables() {
    let markdown = """
        ```swift
        let x = 1
        ```

        > quoted

        ---

        | Name | Cost |
        |:-----|-----:|
        | A    | $1   |
        | B    |
        """
    let blocks = MarkdownBlock.parse(markdown)
    #expect(blocks.count == 4)
    #expect(blocks[0] == .code(language: "swift", code: "let x = 1"))
    guard case .quote(let quoted) = blocks[1], case .paragraph(let text) = quoted.first else {
        Issue.record("no quote")
        return
    }
    #expect(plain(text) == "quoted")
    #expect(blocks[2] == .rule)
    guard case .table(let columns, let header, let rows) = blocks[3] else { Issue.record("no table"); return }
    #expect(columns == [.leading, .trailing])
    #expect(header.map(plain) == ["Name", "Cost"])
    #expect(rows.map { $0.map(plain) } == [["A", "$1"], ["B", ""]])
}

@Test func neverRendersHTMLOrLoadsImages() throws {
    let blocks = MarkdownBlock.parse("<div>raw</div>\n\nSee ![a chart](https://example.com/c.png) <b>here</b>")
    guard case .paragraph(let html) = try #require(blocks.first) else { Issue.record("no paragraph"); return }
    #expect(plain(html) == "<div>raw</div>")
    guard case .paragraph(let text) = blocks[1] else { Issue.record("no paragraph"); return }
    #expect(plain(text) == "See a chart <b>here</b>")
    #expect(text.runs.allSatisfy { $0.imageURL == nil })
    #expect(text.runs.contains { $0.link == URL(string: "https://example.com/c.png") })
}

@Test func anUnclosedFenceIsCode() {
    #expect(MarkdownBlock.parse("```\nstill typing") == [.code(language: nil, code: "still typing")])
}
