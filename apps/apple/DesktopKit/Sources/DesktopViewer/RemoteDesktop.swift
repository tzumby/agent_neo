import CoreGraphics
import Foundation
import Observation
import RFB

/// X11 keysyms the take-over view sends besides typed characters.
public enum Keysym {
    public static let backspace: UInt32 = 0xFF08
    public static let tab: UInt32 = 0xFF09
    public static let returnKey: UInt32 = 0xFF0D
    public static let escape: UInt32 = 0xFF1B
    public static let home: UInt32 = 0xFF50
    public static let left: UInt32 = 0xFF51
    public static let up: UInt32 = 0xFF52
    public static let right: UInt32 = 0xFF53
    public static let down: UInt32 = 0xFF54
    public static let pageUp: UInt32 = 0xFF55
    public static let pageDown: UInt32 = 0xFF56
    public static let end: UInt32 = 0xFF57
    public static let delete: UInt32 = 0xFFFF
    public static let shift: UInt32 = 0xFFE1
    public static let control: UInt32 = 0xFFE3
    public static let alt: UInt32 = 0xFFE9
    public static let superKey: UInt32 = 0xFFEB
}

/// One take-over session: a VNC connection to the desktop that reconnects
/// while the view is open, frames for the view, and input for the desktop.
/// Coordinates are desktop pixels.
@MainActor
@Observable
public final class RemoteDesktop {
    public enum State: Equatable, Sendable {
        case connecting
        case live
        case offline(String)
    }

    public private(set) var state: State = .connecting
    /// The desktop's size in pixels; zero until the first frame.
    public private(set) var size = CGSize.zero
    /// The latest frame. Frames bypass SwiftUI: the view sets `onFrame`.
    @ObservationIgnored public private(set) var frame: CGImage?
    @ObservationIgnored public var onFrame: ((CGImage) -> Void)?

    @ObservationIgnored private var client: RFBClient?

    public init() {}

    /// Runs until the calling task is cancelled. `connect` opens a new,
    /// connected transport for each attempt.
    public func run(connect: @escaping @Sendable () async throws -> any RFBTransport) async {
        while !Task.isCancelled {
            state = .connecting
            do {
                let client = RFBClient(transport: try await connect())
                self.client = client
                try await client.run { [weak self] event in
                    // This runs on the session's task, so copy the frame before
                    // the next update is decoded into the same memory.
                    switch event {
                    case .connected:
                        DispatchQueue.main.async { MainActor.assumeIsolated { self?.state = .live } }
                    case .updated(let framebuffer, _):
                        guard let image = framebuffer.makeImage() else { return client.framebufferConsumed() }
                        DispatchQueue.main.async { MainActor.assumeIsolated { self?.show(image, from: client) } }
                    default:
                        break
                    }
                }
                if !Task.isCancelled { state = .offline("The desktop closed the connection.") }
            } catch {
                if Task.isCancelled { break }
                state = .offline((error as? RFBError)?.description ?? error.localizedDescription)
            }
            client = nil
            try? await Task.sleep(for: .seconds(2))
        }
        client?.close()
        client = nil
    }

    private func show(_ image: CGImage, from source: RFBClient) {
        defer { source.framebufferConsumed() }  // lets the client ask for the next update
        guard source === client else { return }
        frame = image
        let newSize = CGSize(width: image.width, height: image.height)
        if size != newSize { size = newSize }
        if state != .live { state = .live }
        onFrame?(image)
    }

    // MARK: Pointer

    /// Moves the pointer, with the left button held while `pressed` (dragging).
    public func move(x: Int, y: Int, pressed: Bool = false) {
        pointer(x: x, y: y, buttons: pressed ? .left : [])
    }

    public func click(x: Int, y: Int, right: Bool = false) {
        pointer(x: x, y: y, buttons: [])
        pointer(x: x, y: y, buttons: right ? .right : .left)
        pointer(x: x, y: y, buttons: [])
    }

    /// Scrolls by whole wheel clicks; positive is down (or right with `horizontal`).
    public func scroll(x: Int, y: Int, clicks: Int, horizontal: Bool = false) {
        let wheel: RFBButtons =
            horizontal ? (clicks > 0 ? .wheelRight : .wheelLeft) : (clicks > 0 ? .wheelDown : .wheelUp)
        for _ in 0..<abs(clicks) {
            pointer(x: x, y: y, buttons: wheel)
            pointer(x: x, y: y, buttons: [])
        }
    }

    private func pointer(x: Int, y: Int, buttons: RFBButtons) {
        client?.pointer(x: max(0, x), y: max(0, y), buttons: buttons)
    }

    // MARK: Keyboard

    /// Presses and releases a key, holding `modifiers` (keysyms) around it.
    public func key(_ keysym: UInt32, modifiers: [UInt32] = []) {
        guard let client else { return }
        modifiers.forEach { client.key($0, down: true) }
        client.key(keysym, down: true)
        client.key(keysym, down: false)
        modifiers.reversed().forEach { client.key($0, down: false) }
    }

    /// Types text as key presses (newline is Return). Works for any text the
    /// desktop's keymap can produce, so pasting doesn't need the remote clipboard.
    public func type(_ text: String, modifiers: [UInt32] = []) {
        for scalar in text.unicodeScalars {
            guard let keysym = RFBKeyMap.keysym(for: scalar) else { continue }
            key(keysym, modifiers: modifiers)
        }
    }
}
