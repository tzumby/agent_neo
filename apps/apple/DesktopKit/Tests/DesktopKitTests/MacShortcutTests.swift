import DesktopViewer
import Testing

/// ⌘ shortcuts in the Mac's take-over window: Ctrl on the Linux desktop,
/// except the few that belong to the Mac.
@Test func commandShortcutsBecomeControlOnTheDesktop() {
    // ⌘C copies on the desktop, and its clipboard comes back to the Mac.
    #expect(MacShortcut.action(keyCode: 0x08, characters: "c") == .send(keysym: 0x63, control: true, copies: true))
    #expect(MacShortcut.action(keyCode: 0x07, characters: "x") == .send(keysym: 0x78, control: true, copies: true))
    #expect(MacShortcut.action(keyCode: 0x11, characters: "t") == .send(keysym: 0x74, control: true, copies: false))
    // ⇧⌘T: Shift is already down on the desktop.
    #expect(MacShortcut.action(keyCode: 0x11, characters: "T") == .send(keysym: 0x54, control: true, copies: false))
    // ⌘V types the Mac's clipboard instead.
    #expect(MacShortcut.action(keyCode: 0x09, characters: "v") == .paste)
    #expect(MacShortcut.action(keyCode: 0x09, characters: "V") == .paste)
}

@Test func someShortcutsStayOnTheMac() {
    for key in ["q", "w", "h", "m", ",", "`"] {
        #expect(MacShortcut.action(keyCode: 0, characters: key) == .mac, "⌘\(key)")
    }
}

@Test func commandArrowsMoveLikeOnTheMac() {
    #expect(MacShortcut.action(keyCode: 0x7B, characters: "\u{F702}") == .send(keysym: 0xFF50, control: false, copies: false))
    #expect(MacShortcut.action(keyCode: 0x7C, characters: "\u{F703}") == .send(keysym: 0xFF57, control: false, copies: false))
    #expect(MacShortcut.action(keyCode: 0x7E, characters: "\u{F700}") == .send(keysym: 0xFF50, control: true, copies: false))
    #expect(MacShortcut.action(keyCode: 0x7D, characters: "\u{F701}") == .send(keysym: 0xFF57, control: true, copies: false))
    // Function keys keep Ctrl; a key with no keysym sends nothing.
    #expect(MacShortcut.action(keyCode: 0x6F, characters: "\u{F70F}") == .send(keysym: 0xFFC9, control: true, copies: false))
    #expect(MacShortcut.action(keyCode: 0x0A, characters: nil) == nil)
}
