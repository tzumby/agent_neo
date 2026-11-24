/// Bringing what the desktop copied over to this device, only after being asked,
/// so a page can't fill the clipboard. x11vnc sends selections as they change, and
/// nothing new when copying what it already sent. So after a copy: wait a moment
/// for the desktop to send it, else take what it sent last.
struct DesktopCopy {
    private(set) var lastSent: String?
    private var waiting: Int?
    private var count = 0

    /// A copy was pressed. Returns which one, for `expired`.
    mutating func begin() -> Int {
        count += 1
        waiting = count
        return count
    }

    /// The desktop sent text: returns it if a copy is waiting for it.
    mutating func received(_ text: String) -> String? {
        lastSent = text
        guard waiting != nil else { return nil }
        waiting = nil
        return text
    }

    /// The wait is over. `open`: this copy is still waiting (nothing new came,
    /// and no later copy took over), so `text` is what it gets, if anything.
    mutating func expired(_ copy: Int) -> (open: Bool, text: String?) {
        guard waiting == copy else { return (false, nil) }
        waiting = nil
        return (true, lastSent)
    }
}
