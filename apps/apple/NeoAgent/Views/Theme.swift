import AgentAPI
import ChatCore
import SwiftUI

#if os(iOS)
    import UIKit
#else
    import AppKit
#endif

/// A quiet, flat look, light by default (as ChatGPT's apps): white, a few
/// greys, near-black for the main action and the user's messages, hairline
/// edges and soft shadows rather than glass. Each colour has a light and a
/// dark value, for the Dark and System appearances (Settings).
enum Theme {
    /// Behind the chat.
    static let background = Color(light: 0xFFFFFF, dark: 0x212121)
    /// Behind the Mac's conversations.
    static let sidebar = Color(light: 0xF9F9F9, dark: 0x181818)
    /// A sidebar row under the pointer, and the one picked.
    static let hover = Color(light: 0xF0F0F0, dark: 0x262626)
    static let selection = Color(light: 0xE8E8E8, dark: 0x2F2F2F)
    /// Cards.
    static let surface = Color(light: 0xFFFFFF, dark: 0x2A2A2A)
    /// What floats over the chat: the composer, banners.
    static let elevated = Color(light: 0xFFFFFF, dark: 0x303030)
    /// Code, and fields inside cards.
    static let inset = Color(light: 0xF5F5F5, dark: 0x1C1C1C)
    /// Chips and small fills.
    static let chip = Color(light: 0xF0F0F0, dark: 0x3A3A3A)
    /// Edges of cards, fields and the composer.
    static let hairline = Color(light: .black.opacity(0.09), dark: .white.opacity(0.10))
    static let border = Color(light: .black.opacity(0.13), dark: .white.opacity(0.14))
    /// The user's messages: near-black with white text (dark: a lighter grey).
    static let userBubble = Color(light: 0x0D0D0D, dark: 0x3A3A3A)
    static let userText = Color.white
    /// The main action (Send, Approve): near-black, or white in the dark.
    static let prominent = Color(light: 0x0D0D0D, dark: 0xECECEC)
    static let onProminent = Color(light: 0xFFFFFF, dark: 0x0D0D0D)
    /// Inline code's chip.
    static let codeChip = Color(light: .black.opacity(0.06), dark: .white.opacity(0.12))
    /// Yellow that still reads on white (paused, keys).
    static let amber = Color(light: 0xB86E00, dark: 0xFFD60A)
    /// Soft shadow under floating things.
    static let shadow = Color(light: .black.opacity(0.07), dark: .black.opacity(0.35))
    /// The chat's widest column, for a readable line on the Mac (wider than any iPhone).
    static let column: CGFloat = 760
}

extension Color {
    /// One colour in light mode, another in dark, from hex RGB.
    init(light: UInt32, dark: UInt32) {
        self.init(light: Color(hex: light), dark: Color(hex: dark))
    }

    init(light: Color, dark: Color) {
        #if os(iOS)
            self.init(uiColor: UIColor { $0.userInterfaceStyle == .dark ? UIColor(dark) : UIColor(light) })
        #else
            self.init(
                nsColor: NSColor(name: nil) { appearance in
                    appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? NSColor(dark) : NSColor(light)
                })
        #endif
    }

    init(hex: UInt32) {
        self.init(
            .sRGB, red: Double((hex >> 16) & 0xFF) / 255, green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255)
    }
}

/// Light (the default), Dark, or as the system is set. Settings picks it.
enum Appearance: String, CaseIterable, Identifiable {
    case light, dark, system

    static let key = "appearance"

    var id: Self { self }

    var label: String {
        switch self {
        case .light: "Light"
        case .dark: "Dark"
        case .system: "System"
        }
    }

    var scheme: ColorScheme? {
        switch self {
        case .light: .light
        case .dark: .dark
        case .system: nil
        }
    }
}

extension View {
    /// A content card: a white surface with a hairline edge.
    func card() -> some View {
        padding(16)
            .background(Theme.surface, in: .rect(cornerRadius: 20))
            .overlay(RoundedRectangle(cornerRadius: 20).strokeBorder(Theme.hairline, lineWidth: 1))
    }

    /// Something that floats over the chat (the composer, a banner): a
    /// surface with a hairline edge and a soft shadow.
    func floating<S: InsettableShape>(_ shape: S) -> some View {
        background(Theme.elevated, in: shape)
            .overlay(shape.strokeBorder(Theme.hairline, lineWidth: 1))
            .shadow(color: Theme.shadow, radius: 12, y: 3)
    }

    /// Applies the appearance Settings picked: the iPhone's scenes take it
    /// as a preference; the Mac sets it for the whole app (its windows'
    /// preference doesn't go back to the system's once set).
    func appearance(_ appearance: Appearance) -> some View {
        #if os(iOS)
            preferredColorScheme(appearance.scheme)
        #else
            onChange(of: appearance, initial: true) { _, appearance in
                NSApp.appearance =
                    switch appearance {
                    case .light: NSAppearance(named: .aqua)
                    case .dark: NSAppearance(named: .darkAqua)
                    case .system: nil
                    }
            }
        #endif
    }
}

/// Opens the take-over view for an Action needed card.
struct TakeOverAction: Sendable, Equatable {
    let open: @MainActor @Sendable (Action) -> Void

    /// Always equal: the closure is rebuilt on every chat update but does the
    /// same thing, and an unequal environment value would redraw every card.
    static func == (lhs: TakeOverAction, rhs: TakeOverAction) -> Bool { true }
}

/// Scrolls the chat to a task's conversation ("Why do you know this?").
struct OpenTask: Sendable, Equatable {
    let open: @MainActor @Sendable (String) -> Void

    static func == (lhs: OpenTask, rhs: OpenTask) -> Bool { true }
}

// Chat rows get the store, client and take-over action from the environment,
// so each row's only input is its (Equatable) item and SwiftUI redraws a row
// only when that item changes.
extension EnvironmentValues {
    @Entry var chatStore: ChatStore? = nil
    @Entry var agentClient: AgentClient? = nil
    @Entry var takeOver: TakeOverAction? = nil
    @Entry var openTask: OpenTask? = nil
}
