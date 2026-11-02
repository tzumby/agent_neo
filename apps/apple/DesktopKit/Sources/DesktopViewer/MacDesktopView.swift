#if os(macOS)
import AppKit
import RFB
import RFBMetal

/// The desktop in a Mac window: swift-rfb's Metal view, scaled to fit, with
/// the Mac's pointer, scroll wheel and keyboard going to the desktop. ⌘
/// shortcuts become the desktop's Ctrl shortcuts (`MacShortcut`); ⌘V types
/// the Mac's clipboard; after ⌘C or ⌘X, what the desktop copied comes back
/// to the Mac's clipboard (and only then, so a page can't fill it).
@MainActor
public final class MacDesktopView: NSView {
    private let screen: RFBView
    private var monitor: Any?
    /// When ⌘C or ⌘X was last sent, while waiting for the desktop's clipboard.
    private var copying: ContinuousClock.Instant?
    /// The desktop's clipboard as last sent: x11vnc sends nothing new when
    /// copying what it already sent (a selection, say).
    private var lastClipboard: String?

    /// Run the desktop's session with this, so the view draws it.
    public let renderer: RemoteDesktop.Renderer

    public init?(desktop: RemoteDesktop) {
        guard let screen = RFBView() else { return nil }
        self.screen = screen
        renderer = RemoteDesktop.Renderer(
            events: screen.eventHandler,
            attach: { [weak screen] client in screen?.client = client })
        super.init(frame: .zero)
        screen.autoresizingMask = [.width, .height]
        addSubview(screen)
        desktop.onClipboard = { [weak self] text in self?.received(text) }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    public override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
        guard window != nil else { return }
        screen.frame = bounds
        screen.isInputEnabled = true
        window?.makeFirstResponder(screen)
        // Sees keys before the menu does, so ⌘C reaches the desktop as Ctrl+C.
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .flagsChanged]) { [weak self] event in
            let handled = MainActor.assumeIsolated { self?.handled(event) ?? false }
            return handled ? nil : event
        }
    }

    /// Types text on the desktop as key presses (the Paste button).
    public func type(_ text: String) {
        screen.typeText(text)
    }

    /// Whether the event was dealt with here (and so goes no further).
    private func handled(_ event: NSEvent) -> Bool {
        guard let window, event.window === window, window.firstResponder === screen else { return false }
        switch event.type {
        case .flagsChanged:
            // ⌘ itself stays on the Mac (the desktop would get Super).
            return [0x37, 0x36].contains(event.keyCode)
        case .keyDown where event.modifierFlags.contains(.command):
            switch MacShortcut.action(keyCode: event.keyCode, characters: event.charactersIgnoringModifiers) {
            case .mac:
                return false
            case .paste:
                if let text = NSPasteboard.general.string(forType: .string) { screen.typeText(text) }
            case .send(let keysym, let control, let copies):
                send(keysym, control: control)
                if copies { awaitClipboard() }
            case nil:
                break
            }
            return true
        default:
            return false
        }
    }

    private func send(_ keysym: UInt32, control: Bool) {
        guard let client = screen.client else { return }
        let ctrl: UInt32 = 0xFFE3
        if control { client.key(ctrl, down: true) }
        client.key(keysym, down: true)
        client.key(keysym, down: false)
        if control { client.key(ctrl, down: false) }
    }

    /// Waits a moment for the desktop to send what was copied, else takes
    /// what it sent last (the same text, so x11vnc didn't send it again).
    private func awaitClipboard() {
        let started = ContinuousClock.now
        copying = started
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(600))
            guard let self, copying == started else { return }
            copying = nil
            if let lastClipboard { Self.putOnMac(lastClipboard) }
        }
    }

    private func received(_ text: String) {
        lastClipboard = text
        guard let copying, ContinuousClock.now - copying < .seconds(3) else { return }
        self.copying = nil
        Self.putOnMac(text)
    }

    private static func putOnMac(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}
#endif
