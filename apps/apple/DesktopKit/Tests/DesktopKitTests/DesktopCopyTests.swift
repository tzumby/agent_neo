import Testing

@testable import DesktopViewer

@Test func nothingComesBackUntilACopyIsPressed() {
    var copy = DesktopCopy()
    // A page selecting or copying on its own doesn't reach this device's clipboard.
    #expect(copy.received("from a page") == nil)
    _ = copy.begin()
    #expect(copy.received("what I selected") == "what I selected")
    #expect(copy.received("later") == nil, "one copy, one delivery")
}

@Test func copyingWhatWasAlreadySentTakesTheLastText() {
    var copy = DesktopCopy()
    _ = copy.received("selected earlier")
    let pressed = copy.begin()
    let ended = copy.expired(pressed)
    #expect(ended.open && ended.text == "selected earlier")
}

@Test func withNothingSelectedACopySaysSo() {
    var copy = DesktopCopy()
    let ended = copy.expired(copy.begin())
    #expect(ended.open && ended.text == nil)
}

@Test func aFinishedOrReplacedCopyDoesNotDeliverAgain() {
    var copy = DesktopCopy()
    let first = copy.begin()
    #expect(copy.received("text") == "text")
    #expect(copy.expired(first).open == false, "already delivered")
    let second = copy.begin()
    let third = copy.begin()
    #expect(copy.expired(second).open == false, "a later copy took over")
    #expect(copy.expired(third).open)
}
