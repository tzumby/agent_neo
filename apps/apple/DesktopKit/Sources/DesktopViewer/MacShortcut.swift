import RFB

/// What a ⌘ shortcut does in the Mac's take-over window. The desktop runs
/// Linux, where the same shortcuts use Ctrl: ⌘C is Ctrl+C there, ⌘← is Home.
/// A few stay with the Mac (quit, close the window, hide, minimise,
/// Settings, switch windows), and ⌘V types the Mac's clipboard.
public enum MacShortcut: Equatable, Sendable {
    /// The Mac handles it (⌘Q, ⌘W, ⌘H, ⌘M, ⌘, and ⌘`).
    case mac
    /// Type the Mac's clipboard on the desktop.
    case paste
    /// Press `keysym` on the desktop, holding Ctrl when `control`. `copies`:
    /// the desktop's clipboard changes, so bring it to the Mac.
    case send(keysym: UInt32, control: Bool, copies: Bool)

    /// Mac key codes (kVK_*).
    private static let left: UInt16 = 0x7B
    private static let right: UInt16 = 0x7C
    private static let down: UInt16 = 0x7D
    private static let up: UInt16 = 0x7E

    private static let home: UInt32 = 0xFF50
    private static let end: UInt32 = 0xFF57

    /// `characters`: the key's characters ignoring modifiers (Shift aside).
    /// Nil: nothing to send (a key the desktop has no keysym for).
    public static func action(keyCode: UInt16, characters: String?) -> MacShortcut? {
        switch keyCode {
        case left: return .send(keysym: home, control: false, copies: false)
        case right: return .send(keysym: end, control: false, copies: false)
        case up: return .send(keysym: home, control: true, copies: false)
        case down: return .send(keysym: end, control: true, copies: false)
        default: break
        }
        let key = characters?.lowercased() ?? ""
        if ["q", "w", "h", "m", ",", "`"].contains(key) { return .mac }
        if key == "v" { return .paste }
        guard let keysym = RFBKeyMap.keysym(keyCode: keyCode, characters: characters) else { return nil }
        return .send(keysym: keysym, control: true, copies: key == "c" || key == "x")
    }
}
